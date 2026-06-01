// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `seer`: interactive log viewer.
//!
//! Builds a [`seer::Engine`] from the file paths on the command line and
//! presents one or more tabs over it.  Each [`Tab`] is an independent
//! view with its own [`Filter`] and scroll position; the engine itself
//! (and therefore the underlying sources) is shared.

use camino::Utf8PathBuf;
use chrono::{DateTime, Utc};
use clap::Parser;
use ratatui::Frame;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Tabs, Wrap};
use regex::Regex;
#[cfg(test)]
use seer::Event as LogEvent;
use seer::streamview::{Viewport, ViewportStatus};
#[cfg(test)]
use seer::test_fixtures::TestDir;
use seer::{
    Bookmark, BookmarkId, BookmarkName, ByteLen, Cadence, CoreField, Cursor,
    Direction, Engine, EventIdx, EventPredicate, FieldName, Filter, Form,
    HostnameDisplay, LineIdx, LogStream, LogStreamId, MatchKind, Materialized,
    ParseStats, RenderOpts, Row, SavePolicy, SearchAnchor, SearchDir,
    SearchOutcome, Selector, Session, SessionId, SessionMatch, SessionSource,
    SessionStore, SourceId, StoreError, SummaryBuilder, TabKind,
    build_seeit_command, format_summary,
};
#[cfg(test)]
use seer::{EngineEvent, LogStreamPosition};
use std::time::{Duration, Instant};

/// Position of a tab in [`App::tabs`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct TabIdx(usize);

impl TabIdx {
    fn get(self) -> usize {
        self.0
    }

    /// Returns the tab index immediately before this one.  Panics if
    /// `self` is zero — callers use this when patching up indices for
    /// tabs whose position shifted left after a close, so a zero input
    /// would indicate the bug of trying to shift past the start.  The
    /// explicit [`usize::checked_sub`] is so the panic is unconditional;
    /// a bare `self.0 - 1` would wrap to [`usize::MAX`] in release mode.
    fn prev(self) -> TabIdx {
        let Some(prev) = self.0.checked_sub(1) else {
            panic!("TabIdx::prev called on zero index");
        };
        TabIdx(prev)
    }
}

impl std::fmt::Display for TabIdx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Parser)]
#[command(about = "interactive log explorer")]
struct Args {
    /// One or more bunyan log files to read, in order.
    ///
    /// Mutually exclusive with `--resume` and `--list`.
    #[arg(conflicts_with_all = ["resume", "list"])]
    files: Vec<Utf8PathBuf>,

    /// Resume a saved session by id.  Skips the resume dialog and
    /// opens the TUI directly on the loaded session; the engine is
    /// rebuilt from the session's own source list.  Aborts with an
    /// error if any of the session's source files no longer exists.
    #[arg(long, value_name = "SESSION_ID", conflicts_with = "list")]
    resume: Option<SessionId>,

    /// List saved sessions and exit, without opening the TUI.
    #[arg(long)]
    list: bool,
}

/// Registers each user-supplied path with the engine and returns the
/// corresponding [`SessionSource`] rows.
///
/// Each path is canonicalized and stat'd at startup so the saved
/// session captures a stable path-plus-fingerprint pair: a later
/// resume can detect a file whose content has changed since the
/// session was last written.  The [`SourceId`] in each row is the
/// one the engine assigned to the file, so cursors and bookmarks
/// in the session line up with the engine's view of the world.
fn build_session_sources(
    paths: &[Utf8PathBuf],
    engine: &mut Engine,
) -> std::io::Result<iddqd::IdOrdMap<SessionSource>> {
    let mut sources = iddqd::IdOrdMap::new();
    for path in paths {
        let canonical = path.canonicalize_utf8()?;
        let metadata = std::fs::metadata(&canonical)?;
        let size = metadata.len();
        let mtime: DateTime<Utc> = metadata.modified()?.into();
        let id = engine.add_file_source(&canonical)?;
        sources
            .insert_unique(SessionSource { id, path: canonical, mtime, size })
            .expect("Engine::add_file_source returns a fresh SourceId");
    }
    Ok(sources)
}

/// Resolution of the startup dialog.
///
/// The dialog itself is a small modal that runs before the main TUI
/// opens; the variant it returns drives the next step in `main`.
enum StartupChoice {
    /// Reuse this on-disk [`Session`].  The dialog already had a full
    /// [`Session`] in hand (it came from
    /// [`SessionStore::find_matches`]), so no second `load()` is
    /// needed.  Boxed so the enum's other unit variants don't pay
    /// for the `Session`'s bulk.
    ResumeSavedSession(Box<Session>),
    /// Mint a fresh saved session: write to disk, plumb the
    /// [`SessionStore`] into [`App`].
    NewSavedSession,
    /// Mint a fresh session but skip persistence entirely: no
    /// initial save, no [`SessionStore`] on the [`App`], and no
    /// "session saved" hint on exit.
    NewTransientSession,
    /// User dismissed the dialog (Esc / Ctrl-C).  Caller should
    /// exit immediately.
    Quit,
}

/// State of the startup-resume modal.
///
/// Holds the candidate sessions returned from
/// [`SessionStore::find_matches`] plus the user's currently-highlighted
/// row.  The choice space is the candidate list followed by two fixed
/// rows: "new saved" and "new transient".  Empty `matches` is
/// supported and just collapses to the two fixed rows.
struct StartupDialog {
    matches: Vec<SessionMatch>,
    /// Index into the virtual row list: `0..matches.len()` are the
    /// candidate rows; `matches.len()` is "new saved"; the next
    /// index is "new transient".
    selected: usize,
}

impl StartupDialog {
    fn new(matches: Vec<SessionMatch>) -> Self {
        Self { matches, selected: 0 }
    }

    fn rows(&self) -> usize {
        // 2 fixed rows + however many resume candidates.
        self.matches.len() + 2
    }

    fn new_saved_idx(&self) -> usize {
        self.matches.len()
    }

    fn new_transient_idx(&self) -> usize {
        self.matches.len() + 1
    }

    /// Returns the [`StartupChoice`] for the currently-highlighted
    /// row, consuming the dialog so the chosen [`Session`] can be
    /// moved out by value.
    fn confirm(mut self) -> StartupChoice {
        if self.selected < self.matches.len() {
            // `swap_remove` is fine — we are dropping `self` either
            // way, so element-order disturbance is irrelevant.
            let m = self.matches.swap_remove(self.selected);
            StartupChoice::ResumeSavedSession(Box::new(m.session))
        } else if self.selected == self.new_saved_idx() {
            StartupChoice::NewSavedSession
        } else {
            StartupChoice::NewTransientSession
        }
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn move_down(&mut self) {
        let last = self.rows().saturating_sub(1);
        if self.selected < last {
            self.selected += 1;
        }
    }

    /// Routes one keypress.  Returns `Some(choice)` when the user
    /// has finalized a choice (Enter or Esc); `None` when the key
    /// was navigation or unhandled.
    fn handle_key(self, key: KeyEvent) -> StartupDialogStep {
        match key.code {
            KeyCode::Esc => StartupDialogStep::Done(StartupChoice::Quit),
            KeyCode::Char('c')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                StartupDialogStep::Done(StartupChoice::Quit)
            }
            KeyCode::Enter => StartupDialogStep::Done(self.confirm()),
            KeyCode::Char('j') | KeyCode::Down => {
                let mut d = self;
                d.move_down();
                StartupDialogStep::Continue(d)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let mut d = self;
                d.move_up();
                StartupDialogStep::Continue(d)
            }
            // Numeric shortcuts: 1..9 jump to the corresponding
            // resume candidate (1-indexed).  Out-of-range numbers
            // are ignored; the user can still scroll.
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let idx = (c as usize) - ('1' as usize);
                if idx < self.matches.len() {
                    let mut d = self;
                    d.selected = idx;
                    StartupDialogStep::Continue(d)
                } else {
                    StartupDialogStep::Continue(self)
                }
            }
            _ => StartupDialogStep::Continue(self),
        }
    }
}

/// Result of stepping the startup dialog with one keypress.
///
/// `Continue` returns ownership of the dialog so the event loop can
/// re-render it; `Done` signals the user has made (or dismissed)
/// their choice.
enum StartupDialogStep {
    Continue(StartupDialog),
    Done(StartupChoice),
}

/// Human-friendly tag for a [`MatchKind`].
fn match_kind_label(kind: MatchKind) -> &'static str {
    match kind {
        MatchKind::Exact => "exact",
        MatchKind::Superset => "superset",
        MatchKind::Overlap => "overlap",
    }
}

/// Renders the startup dialog into a centered modal pane.
fn render_startup_dialog(frame: &mut Frame, dialog: &StartupDialog) {
    let area = frame.area();
    frame.render_widget(Clear, area);

    // Build all rows up front so we can size the popup to fit the
    // content.  Two trailing rows for "new saved"/"new transient",
    // plus an optional separator row when there are candidates.
    let mut rows: Vec<Line<'_>> = Vec::new();
    for (i, m) in dialog.matches.iter().enumerate() {
        let row = format!(
            "{}  Resume {}  ({} UTC, {} streams, {} sources, {})",
            (i + 1).min(9),
            m.session.id,
            m.session.last_saved_at.format("%Y-%m-%d %H:%M:%S"),
            m.session.streams.len(),
            m.session.sources.len(),
            match_kind_label(m.kind),
        );
        rows.push(highlight_if_selected(row, i == dialog.selected));
    }
    if !dialog.matches.is_empty() {
        rows.push(Line::raw("─".repeat(60)));
    }
    rows.push(highlight_if_selected(
        "   Start a new saved session".to_string(),
        dialog.selected == dialog.new_saved_idx(),
    ));
    rows.push(highlight_if_selected(
        "   Start a transient session (no file written)".to_string(),
        dialog.selected == dialog.new_transient_idx(),
    ));
    // Footer: short key-binding cheat sheet.
    rows.push(Line::raw(""));
    rows.push(Line::raw("j/k or ↑/↓ navigate · enter confirm · esc quit"));

    // Centered popup sized to fit the rows (plus borders and a
    // small horizontal margin).  Capped at the terminal size.
    let content_h = (rows.len() as u16) + 2; // + 2 for borders
    let content_w =
        rows.iter().map(|line| line.width() as u16).max().unwrap_or(40) + 4; // + 4 for borders + margin
    let h = content_h.min(area.height);
    let w = content_w.min(area.width);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };

    let title = if dialog.matches.is_empty() {
        " seer — no saved sessions match these files "
    } else {
        " seer — saved sessions for these files "
    };
    let block = Block::bordered().title(title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(rows), inner);
}

/// Wraps `text` in a `Line` whose style flips to a highlighted bar
/// when `selected` is true.
fn highlight_if_selected(text: String, selected: bool) -> Line<'static> {
    let line = Line::from(text);
    if selected {
        line.style(
            Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD),
        )
    } else {
        line
    }
}

/// Drives the startup-resume dialog in its own ratatui session and
/// returns the user's choice.
///
/// Owns the [`TerminalGuard`] for the dialog's duration; the caller
/// receives the dialog's result, then opens a fresh terminal for the
/// main TUI.  Calling `ratatui::init` twice (once here, once in
/// `run_tui`) can briefly flash; in practice the dialog frame is
/// drawn within a few milliseconds so the seam is invisible on a
/// reasonable terminal.
fn run_startup_dialog(
    matches: Vec<SessionMatch>,
) -> Result<StartupChoice, Box<dyn std::error::Error>> {
    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;
    let mut dialog = StartupDialog::new(matches);
    loop {
        terminal.draw(|frame| render_startup_dialog(frame, &dialog))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match dialog.handle_key(key) {
            StartupDialogStep::Continue(d) => dialog = d,
            StartupDialogStep::Done(choice) => return Ok(choice),
        }
    }
}

/// Returns the saved sessions in the store sorted by `last_saved_at`
/// descending, skipping any whose JSON failed to parse.  A parse
/// error is silently dropped here for the same reason `find_matches`
/// drops them: an unloadable session is not usefully listable
/// either, and the file stays on disk for a human to investigate.
fn load_all_sessions(store: &SessionStore) -> Result<Vec<Session>, StoreError> {
    let ids = store.list()?;
    let mut sessions = Vec::with_capacity(ids.len());
    for id in ids {
        if let Ok(s) = store.load(id) {
            sessions.push(s);
        }
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_saved_at));
    Ok(sessions)
}

/// Truncates `s` to at most `max_len` characters by chopping the
/// *front* and prefixing `...`.  Path-aware: keeps the tail of the
/// path (the filename + a bit of the directory) since that is
/// usually what disambiguates the row.
fn truncate_path_head(s: &str, max_len: usize) -> String {
    let count = s.chars().count();
    if count <= max_len {
        return s.to_string();
    }
    // Take the trailing `max_len - 3` chars; prefix `...`.
    let skip = count - (max_len - 3);
    let tail: String = s.chars().skip(skip).collect();
    format!("...{tail}")
}

/// Pure function: formats the saved-session table the way `--list`
/// prints it.  Returns one line per session plus a header line; an
/// empty input returns a single `"(no saved sessions)"` line so
/// callers don't have to special-case the empty case.
fn format_session_list(sessions: &[Session]) -> String {
    if sessions.is_empty() {
        return "(no saved sessions)\n".to_string();
    }
    // Right-pad ids so the table aligns; the id is fixed-width (8
    // hex chars) but we own the surrounding formatting.
    let mut out = String::new();
    out.push_str(
        "ID        LAST SAVED (UTC)     STREAMS  SOURCES  FIRST SOURCE\n",
    );
    for s in sessions {
        let first_source =
            s.sources.first().map(|src| src.path.as_str()).unwrap_or("(none)");
        let truncated = truncate_path_head(first_source, 60);
        out.push_str(&format!(
            "{}  {}  {:>7}  {:>7}  {}\n",
            s.id,
            s.last_saved_at.format("%Y-%m-%d %H:%M:%S"),
            s.streams.len(),
            s.sources.len(),
            truncated,
        ));
    }
    out
}

/// Prints the saved-session table to stdout.  Backs `--list`.
fn list_sessions(
    store: &SessionStore,
) -> Result<(), Box<dyn std::error::Error>> {
    let sessions = load_all_sessions(store)?;
    print!("{}", format_session_list(&sessions));
    Ok(())
}

/// Builds an [`Engine`] over the source set the resumed [`Session`]
/// captured.  Verifies every recorded path still exists on disk;
/// missing paths are collected and reported in a single error so
/// the user sees the full list at once.
fn engine_for_resumed_session(
    session: &Session,
) -> Result<Engine, Box<dyn std::error::Error>> {
    let mut missing = Vec::new();
    for src in &session.sources {
        if !src.path.exists() {
            missing.push(src.path.clone());
        }
    }
    if !missing.is_empty() {
        let names: Vec<String> =
            missing.iter().map(|p| p.to_string()).collect();
        return Err(format!(
            "cannot resume session {}: missing source files: {}",
            session.id,
            names.join(", "),
        )
        .into());
    }
    let mut engine = Engine::new();
    for src in &session.sources {
        engine.add_file_source(&src.path)?;
    }
    Ok(engine)
}

/// Outcome of [`run_tui`] plus the metadata `main` needs to choose
/// the right exit message.  Carrying the data through a struct
/// keeps every code path (positional files, `--resume`, dialog
/// choice) on the same final-flush + exit-message logic.
struct RunOutcome {
    app: App,
    session_id: SessionId,
    /// `true` when no store was attached for the run (transient
    /// session).  Final flush is a no-op and no resume hint prints.
    transient: bool,
    /// `true` when the run started by resuming a saved session
    /// (either via `--resume` or via the dialog).  Drives the
    /// "continued" vs "saved" wording in the exit hint.
    resumed: bool,
}

/// Runs the TUI to completion and then handles the final flush and
/// exit-message printing.  Shared by both CLI dispatch paths
/// (positional files and `--resume`).
fn run_with_session(
    engine: Engine,
    session: Session,
    store: Option<SessionStore>,
    resumed: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_id = session.id;
    let transient = store.is_none();
    let mut policy = SavePolicy::new(SavePolicy::DEFAULT_DEBOUNCE);
    if !transient {
        // The on-disk file is up to date (we just saved a fresh
        // session, or we just loaded a resumed one), so the
        // debounce window starts now.
        policy.mark_saved(Instant::now());
    }

    let app = run_tui(engine, session, store, policy)?;
    let outcome = RunOutcome { app, session_id, transient, resumed };
    report_exit(outcome);
    Ok(())
}

/// Flushes any final dirty state and prints the exit hint.  Split
/// out so the post-TUI bookkeeping is exercised by one shared
/// function regardless of how the run was started.
fn report_exit(mut outcome: RunOutcome) {
    let final_save_err = if outcome.app.policy.dirty() {
        outcome.app.try_save_now().err()
    } else {
        None
    };

    if outcome.transient {
        // Transient sessions print nothing on a clean exit.
    } else if let Some(err) = final_save_err {
        eprintln!("seer: final session save failed: {err}");
        eprintln!(
            "session id: {} (state on disk may be partial)",
            outcome.session_id
        );
    } else if outcome.resumed {
        println!(
            "session continued.  resume again with: seer --resume {}",
            outcome.session_id
        );
    } else {
        println!(
            "session saved.  resume with: seer --resume {}",
            outcome.session_id
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let store = SessionStore::open()?;

    // `--list`: print the saved-session table and exit.  Mutually
    // exclusive with everything else.
    if args.list {
        return list_sessions(&store);
    }

    // `--resume`: load the named session, rebuild the engine from
    // its own source list, then jump straight into the TUI without
    // a dialog.  Missing source files abort with a clear error.
    if let Some(id) = args.resume {
        let session = store.load(id)?;
        let engine = engine_for_resumed_session(&session)?;
        return run_with_session(
            engine,
            session,
            Some(store),
            /* resumed = */ true,
        );
    }

    // Without --list or --resume, we should be given a list of files to load
    // into a session.
    if args.files.is_empty() {
        return Err("no files provided; use --list to see saved sessions or \
             --resume <id> to reopen one"
            .into());
    }

    // Register every CLI-supplied file with the engine and capture
    // path / mtime / size for the session's source manifest.  The
    // assignment is in lockstep so the engine and session agree on
    // the SourceId for each file.
    let mut engine = Engine::new();
    let sources = build_session_sources(&args.files, &mut engine)?;

    // Look for saved sessions whose source set overlaps the user's
    // command-line paths.  The dialog runs even when no candidates
    // exist so the saved-vs-transient choice is still in front of
    // the user.
    let user_paths: Vec<Utf8PathBuf> =
        sources.iter().map(|s| s.path.clone()).collect();
    let matches = store.find_matches(&user_paths)?;

    let choice = run_startup_dialog(matches)?;

    // Build the [`Session`] the App will use, plus the optional
    // store: transient sessions get `None` and skip every write.
    let (session, store_for_app, resumed) = match choice {
        StartupChoice::Quit => return Ok(()),
        StartupChoice::ResumeSavedSession(s) => (*s, Some(store), true),
        StartupChoice::NewSavedSession => {
            let mut s = Session::new();
            s.sources = sources;
            // Initial save before opening the TUI.  If the state
            // directory isn't writable the user hears about it now,
            // not after typing.
            store.save(s.id, &s)?;
            (s, Some(store), false)
        }
        StartupChoice::NewTransientSession => {
            let mut s = Session::new();
            s.sources = sources;
            (s, None, false)
        }
    };

    run_with_session(engine, session, store_for_app, resumed)
}

/// Runs the ratatui event loop and returns the [`App`] when the user
/// quits, so the caller can inspect the final session state (and
/// trigger a final save) after the terminal has been restored.
fn run_tui(
    engine: Engine,
    session: Session,
    store: Option<SessionStore>,
    policy: SavePolicy,
) -> Result<App, Box<dyn std::error::Error>> {
    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;
    let mut app = App::new_with_session(engine, session, store, policy);
    while !app.quit {
        // Flush any debounced changes whose window has elapsed.
        // Cheap when nothing is dirty; the steady-state loop runs
        // roughly 10×/s in the idle case (event::poll(100ms)), so
        // the debounce only ever slips by a fraction of a second.
        app.flush_if_due();
        terminal.draw(|frame| render(frame, &mut app))?;
        let poll_duration = if app.is_busy() {
            // Long-op mode: advance one chunk per loop iteration and
            // poll briefly for input.  The chunk size is tuned so a
            // single advance returns in well under a frame; the
            // zero-duration poll lets a Ctrl-C land between chunks
            // without slowing the steady state.  Other keys are
            // intentionally ignored — the user is waiting on the
            // active op and surprise key reactions would race the op
            // anyway.
            app.do_work();
            Duration::ZERO
        } else {
            Duration::from_millis(100)
        };

        if event::poll(poll_duration)?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(key)
        }
    }
    Ok(app)
}

/// Restores the terminal on drop so panics and `?`-returns don't leave
/// the user's shell in raw mode / alt-screen.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// Default viewport height used when constructing a [`Tab`] before
/// the actual terminal size is known.  The first render call replaces
/// this with the real height via [`Tab::maintain_window`].  Set high
/// enough to fill any reasonable terminal so the initial fetch covers
/// the visible area; the streamview will extend further if needed.
const INITIAL_VIEWPORT_HEIGHT: u16 = 80;

/// Returns the number of visual rows a `formatted` line will occupy
/// when wrapped at `width` columns.
///
/// Approximates ratatui's `Paragraph::wrap` row count via
/// `chars().count() / width` — accurate for the long unbroken JSON
/// strings produced by raw mode (no whitespace to wrap on, so
/// `break_words` falls back to column-boundary breaks) and within a
/// row or two for whitespace-rich text.  An empty line is one row.
/// `width == 0` collapses every line to one row (degenerate terminal
/// size: don't divide by zero, and the user can't see anything
/// anyway).
fn visual_rows_for(line: &str, width: u16) -> usize {
    if width == 0 {
        return 1;
    }
    line.chars().count().div_ceil(width as usize).max(1)
}

/// Splits `text` into successive slices that are each at most `width`
/// characters wide, breaking at the column boundary regardless of word
/// boundaries.  Used when rendering raw log entries so that a wrapped line can
/// be re-joined by stripping newlines.  (With word-wrap, the wrap point can
/// replace a space, leaving the result ambiguous about whether the newline
/// should re-expand to a space.  With column-wrap, the visual rows are
/// contiguous bytes of the source line.  Stripping newlines recovers the
/// original.)
fn column_chunks(text: &str, width: u16) -> Vec<&str> {
    if width == 0 || text.is_empty() {
        return vec![text];
    }
    let width = width as usize;
    let mut chunks = Vec::new();
    let mut start_byte = 0;
    let mut chars_in_chunk = 0;
    // Walk by chars so we never split a multi-byte UTF-8 sequence.
    for (byte_idx, _) in text.char_indices() {
        if chars_in_chunk == width {
            chunks.push(&text[start_byte..byte_idx]);
            start_byte = byte_idx;
            chars_in_chunk = 0;
        }
        chars_in_chunk += 1;
    }
    chunks.push(&text[start_byte..]);
    chunks
}

// XXX-dap rename all the StreamViews
/// Builds a Stream-tab's initial render state by populating a
/// [`StreamView`] window.  The streamview eagerly maintains its own
/// flat [`Materialized`] cache, so the caller doesn't need to pull
/// out a separate copy.
fn build_viewport(
    engine: &Engine,
    filter: &Filter,
    opts: RenderOpts,
    _viewport_height: u16,
) -> Viewport {
    Viewport::new(engine, filter.clone(), opts)
}

/// Extracts the trailing integer from a default-shaped tab name like
/// `"Tab 7"` or `"Summary 12"`.  Used on resume to bump
/// `App::next_tab_number` past every restored name, so a newly-pushed
/// tab doesn't collide with a name already in use.  Returns `None`
/// for names that don't fit the `Word N` shape (e.g. user-renamed
/// tabs), which is fine: a renamed tab is by definition no longer
/// competing for default-shaped names.
fn parse_tab_number(name: &str) -> Option<usize> {
    let (_, n) = name.rsplit_once(' ')?;
    n.parse().ok()
}

/// Placeholder rows for a Summary tab whose build hasn't run yet.
/// Surfaced by [`Tab::new`] when constructing a Summary tab and by
/// [`App::start_summary_build`] when an existing tab is being
/// rebuilt.  Rendered as a single "Computing summary..." line so the
/// pane isn't visually empty while the [`LongOp`] populates the real
/// histogram; the progress bar lives below in the parse-stats row.
fn summary_placeholder_rows() -> Materialized {
    Materialized {
        events: Vec::new(),
        formatted: vec!["Computing summary...".to_string()],
        event_for_line: vec![EventIdx::ZERO],
        first_line_for_event: vec![LineIdx::ZERO],
        parse_stats: ParseStats::default(),
    }
}

/// Formats a byte count using binary (KiB/MiB/GiB) prefixes.  The 1024
/// boundary keeps "below 1 KiB" displays as raw bytes (`"512 B"`);
/// above that we shift to one decimal place since whole-prefix
/// granularity is too coarse on a screen the user is watching tick by.
fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} B")
    } else if b < MIB {
        format!("{:.1} KiB", b / KIB)
    } else if b < GIB {
        format!("{:.1} MiB", b / MIB)
    } else {
        format!("{:.1} GiB", b / GIB)
    }
}

/// Bytes-per-second variant of [`format_bytes`].  Floors to a B/sec
/// whole number under 1 KiB to avoid noisy `"341.0 B/sec"` displays
/// where the trailing decimal carries no information.
fn format_byte_rate(bytes_per_sec: f64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    if bytes_per_sec < KIB {
        format!("{:.0} B/sec", bytes_per_sec)
    } else if bytes_per_sec < MIB {
        format!("{:.1} KiB/sec", bytes_per_sec / KIB)
    } else if bytes_per_sec < GIB {
        format!("{:.1} MiB/sec", bytes_per_sec / MIB)
    } else {
        format!("{:.1} GiB/sec", bytes_per_sec / GIB)
    }
}

/// Renders the always-shown user-facing status line under each tab.
///
/// The format is:
///
/// ```text
/// Showing N records from byte offset B of T (P%)  ·  (end of stream)
/// ```
///
/// where N is the number of records currently visible, B is the byte
/// offset of the topmost visible record across all sources (a single
/// number summed from the streamview anchor cursor), T is the total
/// bytes of all sources matched by the active filter, and P is
/// `100 * B / T`.
///
/// Tabs without a streamview (synthetic test fixtures and Summary
/// builds before they materialize) have no meaningful byte offset, so
/// the byte half is dropped and the beginning-of-stream marker is
/// suppressed.  The end-of-stream marker still surfaces in that case
/// because `at_eof` is derived from the materialization, not the
/// streamview.
///
/// Summary tabs are a separate case: their rows are histogram entries,
/// not records, so the "Showing N records …" framing doesn't apply
/// and `event_for_line` is empty (the line→event map only exists for
/// tabs whose lines come from real events).  Those tabs get a row-
/// position string instead.
fn format_user_status(
    tab: &Tab,
    engine: &Engine,
    filter: &Filter,
    top: usize,
    bottom: usize,
    at_eof: bool,
) -> String {
    let total_lines = tab.formatted().len();
    if tab.kind == TabKind::Summary {
        let mut s = if total_lines == 0 {
            "Summary: no rows".to_string()
        } else {
            format!(
                "Showing summary rows {}-{} of {}",
                top + 1,
                bottom,
                total_lines
            )
        };
        if at_eof {
            s.push_str("  ·  (end of summary)");
        }
        return s;
    }
    let records_shown = if top < bottom && bottom <= tab.event_for_line().len()
    {
        tab.event_for_line()[bottom - 1].get() - tab.event_for_line()[top].get()
            + 1
    } else {
        0
    };
    let total_bytes = engine.filtered_total_bytes(filter).get();
    let offset_bytes: Option<u64> = tab
        .viewport
        .as_ref()
        .and_then(|v| v.cursor_at_anchor())
        .map(|c| c.byte_offset().get());

    let mut s = match offset_bytes {
        Some(b) if total_bytes > 0 => {
            let pct = (b as f64 / total_bytes as f64) * 100.0;
            format!(
                "Showing {} records from byte offset {} of {} ({:.0}%)",
                records_shown,
                format_bytes(b),
                format_bytes(total_bytes),
                pct,
            )
        }
        _ => format!("Showing {records_shown} records"),
    };
    // EOF wins over BOF when somehow both could be true (a tiny stream
    // where the only visible record is the first and the last): reaching
    // the end is the more notable state.
    if at_eof {
        s.push_str("  ·  (end of stream)");
    } else if offset_bytes == Some(0) && records_shown > 0 {
        s.push_str("  ·  (beginning of stream)");
    }
    s
}

/// Renders a [`ParseStats`] as the developer-oriented fetch-progress
/// row revealed by `p`.  The byte count reflects bytes *scanned*
/// (including those from filter-rejected records) so it tracks the
/// work the engine had to do while the user waited, not the size of
/// what survived.  When the fetch finished in zero measurable time
/// (empty engine, all sources excluded by the source-id filter) the
/// rate half is dropped — it would either divide by zero or be
/// meaningless.
fn format_fetch_stats(stats: &ParseStats) -> String {
    let secs = stats.elapsed.as_secs_f64();
    let bytes = format_bytes(stats.walked_bytes.get());
    if stats.records == 0 || secs <= 0.0 {
        return format!(
            "{} records ({}) fetched in {:.3}s",
            stats.records, bytes, secs,
        );
    }
    let rps = stats.records as f64 / secs;
    let bps = stats.walked_bytes.get() as f64 / secs;
    format!(
        "{} records ({}) fetched in {:.3}s ({:.1} records/sec, {})",
        stats.records,
        bytes,
        secs,
        rps,
        format_byte_rate(bps),
    )
}

/// Records processed per [`LongOp::advance`] chunk.  Sized so that even
/// on slow machines a chunk completes in well under the 16 ms it would
/// take to drop a frame at 60 Hz, while still being large enough that
/// the per-chunk overhead (rebuilding a [`Stepper`] from the saved
/// [`Cursor`] for summary builds, handing back to the event loop for
/// search) doesn't dominate the parse cost.
const LONG_OP_CHUNK_RECORDS: usize = 4_000;

/// A multi-chunk operation driven by the main event loop in between
/// frame draws.  This is used for anything where the TUI would be otherwise
/// unresponsive for an unbounded amount of time, which basically means any time
/// an indefinite amount of scanning is required (e.g., building a summary,
/// searching, seeking to a specific spot).
///
/// While a [`LongOp`] is active, the parse-stats line is replaced with
/// a progress bar; Ctrl-C cancels and unwinds (no partial summary, no
/// anchor change for search).
enum LongOp {
    // Box the [`SummaryOp`] to keep this enum compact.
    BuildSummary(Box<SummaryOp>),
    Search(SearchOp),
    /// Chunked window-fill behind `g`, `G`, bookmark navigation, and
    /// filter rebuild.  See [`SeekOp`].
    Seek(SeekOp),
}

impl LongOp {
    /// Bytes processed so far across all sources.  Drives the numerator
    /// of the progress bar.
    fn bytes_done(&self) -> ByteLen {
        match self {
            LongOp::BuildSummary(op) => op.bytes_read,
            LongOp::Search(op) => op.bytes_done(),
            LongOp::Seek(op) => op.bytes_done,
        }
    }

    /// Total bytes the operation would process if it ran to completion.
    /// Drives the denominator of the progress bar.
    fn total_bytes(&self) -> ByteLen {
        match self {
            LongOp::BuildSummary(op) => op.total_bytes,
            LongOp::Search(op) => op.total_bytes,
            LongOp::Seek(op) => op.total_bytes,
        }
    }

    /// Records observed so far.  For Summary, filter-passing records
    /// folded into the builder; for Search, records walked; for Seek,
    /// records cached so far by the in-flight window-fill.
    fn records(&self) -> u64 {
        match self {
            LongOp::BuildSummary(op) => op.records,
            LongOp::Search(op) => op.records,
            LongOp::Seek(op) => op.records,
        }
    }

    /// Verb shown in the progress bar.  Built-ins are static; Seek
    /// picks its label at construction so `g`/`G`/filter-rebuild can
    /// each surface what they're doing.
    fn label(&self) -> &str {
        match self {
            LongOp::BuildSummary(_) => "Computing summary",
            LongOp::Search(_) => "Searching",
            LongOp::Seek(op) => op.label.as_str(),
        }
    }

    /// True iff this op writes its result back to the tab at
    /// `tab_idx`.  Used by the renderer to decide whether to swap the
    /// progress bar in for the active tab's parse-stats line.
    fn targets_tab(&self, tab_idx: TabIdx) -> bool {
        match self {
            LongOp::BuildSummary(op) => op.tab_idx == tab_idx,
            LongOp::Search(op) => op.tab_idx == tab_idx,
            LongOp::Seek(op) => op.tab_idx == tab_idx,
        }
    }
}

/// In-progress Summary build.  Holds the partial [`SummaryBuilder`]
/// across chunks plus a [`Cursor`] from which the next [`Stepper`] is
/// built.  Drained by repeated calls to [`Self::advance`].  On
/// completion, [`Self::finalize`] turns it into the histogram lines
/// the destination tab will display.
struct SummaryOp {
    /// Index in `App.tabs` of the Summary tab being built.  Captured at
    /// creation; the tab itself is left with empty `formatted` until
    /// `finalize` runs.
    tab_idx: TabIdx,
    filter: Filter,
    builder: SummaryBuilder,
    cursor: Cursor,
    bytes_read: ByteLen,
    records: u64,
    total_bytes: ByteLen,
    started: Instant,
    eof: bool,
}

impl SummaryOp {
    fn new(tab_idx: TabIdx, filter: Filter, total_bytes: ByteLen) -> Self {
        Self {
            tab_idx,
            filter,
            builder: SummaryBuilder::default(),
            cursor: Cursor::new(),
            bytes_read: ByteLen::ZERO,
            records: 0,
            total_bytes,
            started: Instant::now(),
            eof: false,
        }
    }

    /// Folds up to [`LONG_OP_CHUNK_RECORDS`] more records (parsed
    /// successfully and accepted by the filter) into the builder.
    /// Returns `true` once the merge is exhausted and the caller should
    /// finalize.
    fn advance(&mut self, engine: &Engine) -> bool {
        if self.eof {
            return true;
        }
        let mut stepper = engine.stepper(self.filter.clone(), &self.cursor);
        let mut count = 0;
        while count < LONG_OP_CHUNK_RECORDS {
            let Some(rec) = stepper.step_forward() else {
                self.eof = true;
                break;
            };
            self.bytes_read += rec.length();
            if let Ok(event) = rec.event() {
                self.builder.observe(event);
                self.records += 1;
            }
            count += 1;
        }
        self.cursor = stepper.cursor();
        self.eof
    }

    /// Consumes the in-progress build and returns the histogram rows.
    /// Caller installs the result into the destination tab's
    /// [`Tab::standalone_materialized`] slot.
    fn finalize(self) -> Materialized {
        let elapsed = self.started.elapsed();
        let summary = self.builder.finish();
        let formatted = format_summary(&summary);
        let parse_stats = ParseStats {
            records: self.records,
            walked_bytes: self.bytes_read,
            elapsed,
        };
        Materialized {
            events: Vec::new(),
            formatted,
            event_for_line: Vec::new(),
            first_line_for_event: Vec::new(),
            parse_stats,
        }
    }
}

/// In-progress search.  Wraps repeated calls to
/// [`StreamView::search_step_with_budget`]: each chunk consumes one
/// budget's worth of records, and the op auto-resumes through
/// [`SearchOutcome::BudgetExhausted`] until a match is found, the
/// stream is exhausted, or the user cancels.  Records and bytes are
/// taken from the streamview's own parse-stats counters by diffing
/// against a snapshot captured at op start.
struct SearchOp {
    tab_idx: TabIdx,
    regex: Regex,
    direction: SearchDir,
    /// Whether the very first chunk skips the anchor row so `n` after
    /// a previous match advances rather than re-finding it.  Reset to
    /// [`SearchAnchor::Include`] after the first chunk runs.
    anchor: SearchAnchor,
    /// `parse_stats.walked_bytes` from the streamview at op start.
    /// Subtract from the current value to get bytes scanned by *this*
    /// op — the numerator of the progress bar.
    walked_bytes_at_start: ByteLen,
    /// `parse_stats.records` at op start; same idea as
    /// `walked_bytes_at_start`.
    records_at_start: u64,
    total_bytes: ByteLen,
    /// Result of the last advance.  `None` while still searching;
    /// `Some(outcome)` once the op has finished and is awaiting
    /// finalize.  Note that `BudgetExhausted` is *not* terminal here —
    /// the op consumes it internally and re-issues the call.
    outcome: Option<SearchOutcome>,
    /// Live snapshot of the streamview's running parse stats, copied
    /// out at the end of every `advance`.  The streamview itself is
    /// borrowed only during the chunk; the progress bar reads from
    /// here in between.
    bytes_done: ByteLen,
    records: u64,
}

impl SearchOp {
    fn new(
        tab_idx: TabIdx,
        regex: Regex,
        direction: SearchDir,
        anchor: SearchAnchor,
        walked_bytes_at_start: ByteLen,
        records_at_start: u64,
        total_bytes: ByteLen,
    ) -> Self {
        Self {
            tab_idx,
            regex,
            direction,
            anchor,
            walked_bytes_at_start,
            records_at_start,
            total_bytes,
            outcome: None,
            bytes_done: ByteLen::ZERO,
            records: 0,
        }
    }

    fn bytes_done(&self) -> ByteLen {
        self.bytes_done
    }
}

/// What to do once a [`SeekOp`]'s chunked window-fill completes.
///
/// The variant tracks what the user originally asked for so the
/// finalize step can install the right anchor and, for the
/// cursor-driven path, run the synchronous PinBack fallback when
/// forward-from-cursor produced no records.
enum SeekFinalize {
    /// Anchor on `records.front()` line 0 — what `g` (seek to start)
    /// and an anchor-less filter rebuild want.
    Front,
    /// Anchor on `records.back()` last line — what `G` (seek to end)
    /// wants.
    Back,
    /// Anchor on `records.front()` line 0; if the window came up
    /// empty, fall back to a PinBack pop synchronously.  Mirrors the
    /// inline fallback in [`StreamView::seek_to_cursor`] but defers
    /// it to finalize so the long-op driver doesn't have to
    /// pump two phases.
    FrontOrBackFallback,
}

/// In-progress chunked window-fill behind a seek (`g`/`G`/bookmark
/// navigation) or filter rebuild.  Each [`Self::advance`] tick calls
/// [`StreamView::ensure_window_step`] once, fetching one batch, then
/// yields so the renderer can update the progress bar.
struct SeekOp {
    tab_idx: TabIdx,
    /// What kind of finalize work to do once the window-fill is done.
    finalize: SeekFinalize,
    /// Verb shown in the progress bar.  Owned `String` rather than a
    /// `&'static str` so the variants can pick context-specific text
    /// like "Loading view" vs "Applying filter" without inflating
    /// [`SeekFinalize`].
    label: String,
    /// Baseline `parse_stats.walked_bytes` snapshot, subtracted from
    /// the streamview's running totals so the progress bar shows just
    /// what *this* op consumed.  We track walked bytes (including
    /// filter-rejected records) rather than just matching-record
    /// bytes so the bar still ticks during sparse-filter regions
    /// where the scan walks many records without surfacing any.
    walked_bytes_at_start: ByteLen,
    records_at_start: u64,
    bytes_done: ByteLen,
    records: u64,
    total_bytes: ByteLen,
    /// Set once `ensure_window_step` returns `Done`; the next
    /// `advance_long_op` tick finds this and dispatches to
    /// `finalize_long_op`.
    complete: bool,
}

impl SeekOp {
    fn new(
        tab_idx: TabIdx,
        finalize: SeekFinalize,
        label: impl Into<String>,
        walked_bytes_at_start: ByteLen,
        records_at_start: u64,
        total_bytes: ByteLen,
    ) -> Self {
        Self {
            tab_idx,
            finalize,
            label: label.into(),
            walked_bytes_at_start,
            records_at_start,
            bytes_done: ByteLen::ZERO,
            records: 0,
            total_bytes,
            complete: false,
        }
    }
}

/// Formats a [`LongOp`]'s running progress as a single-line status
/// string sized to fit `width` columns.  Replaces [`format_user_status`]
/// in the user status row while the op is in flight; on completion the
/// user status line takes over again on the next frame.
///
/// Layout, from left to right:
///
/// 1. The verb ("Computing summary"/"Searching") followed by a colon.
/// 2. A bracketed bar showing percentage.
/// 3. The percentage as a number.
/// 4. Bytes-done / total-bytes.
/// 5. Records-so-far.
///
/// The bar shrinks (and at very narrow widths is dropped entirely) so
/// the numeric components are never truncated — losing the percent
/// number is more confusing than losing bar resolution.
fn format_long_op_progress(op: &LongOp, width: usize) -> String {
    let bytes_done = op.bytes_done();
    let total_bytes = op.total_bytes();
    let pct = if total_bytes == ByteLen::ZERO {
        100.0
    } else {
        // Cap at 100 — search ops that walk back-fetch buffers (or
        // hypothetical accounting drift) can otherwise overshoot.
        ((bytes_done.get() as f64 / total_bytes.get() as f64) * 100.0)
            .min(100.0)
    };
    let label = op.label();
    let numbers = format!(
        "{:>5.1}%   {} / {}   {} records",
        pct,
        format_bytes(bytes_done.get()),
        format_bytes(total_bytes.get()),
        op.records(),
    );
    let prefix = format!("{label}: ");
    // Compute remaining width for the bar.  Reserve two extra cells
    // for the brackets `[ ... ]` and one for the trailing space
    // between bar and numbers.  When the terminal is too narrow to
    // fit a useful bar, drop it and print just the prefix + numbers.
    let fixed = prefix.chars().count() + numbers.chars().count();
    const MIN_BAR_INNER: usize = 8;
    let bar = if width >= fixed + MIN_BAR_INNER + 3 {
        let bar_inner = width - fixed - 3;
        format!("[{}] ", progress_bar_inner(pct, bar_inner))
    } else {
        String::new()
    };
    format!("{prefix}{bar}{numbers}")
}

/// Renders the inner portion of the progress bar (no brackets) into a
/// `width`-cell string.  Filled cells use the block character `█`;
/// the unfilled tail is space-padded so the bar's right edge is
/// stable as the percentage climbs.  A non-zero percentage always
/// fills at least one cell — otherwise a long-running op spends its
/// first chunks looking like it's making no progress.
fn progress_bar_inner(pct: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let filled_f = (pct / 100.0) * width as f64;
    let mut filled = filled_f as usize;
    if pct > 0.0 && filled == 0 {
        filled = 1;
    }
    if filled > width {
        filled = width;
    }
    let mut s = String::with_capacity(width);
    for _ in 0..filled {
        s.push('\u{2588}'); // FULL BLOCK
    }
    for _ in filled..width {
        s.push(' ');
    }
    s
}

/// Indices of every row in `rows` containing at least one match for
/// `regex`.  Output is naturally sorted ascending.
fn compute_matches(rows: &[String], regex: &Regex) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(i, r)| regex.is_match(r).then_some(i))
        .collect()
}

/// Direction of a `less`-style search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchDirection {
    Forward,
    Backward,
}

impl SearchDirection {
    fn opposite(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
        }
    }

    /// Character drawn at the left of the search prompt — `/` for
    /// forward, `?` for backward.  Matches the key that opened the
    /// dialog.
    fn prompt(&self) -> char {
        match self {
            Self::Forward => '/',
            Self::Backward => '?',
        }
    }
}

/// State of the active search on a tab.
///
/// `matches` is the precomputed list of line indices in
/// [`Tab::formatted`] where `regex` finds at least one match.
/// Populated only for tabs without a [`StreamView`] (test fixtures and
/// Summary tabs); production stream tabs use [`StreamView::search_step`]
/// to navigate, scanning records lazily as needed, and `matches` stays
/// empty.  Render uses `regex` to highlight matches in the visible
/// window for both paths.
struct TabSearch {
    pattern: String,
    regex: Regex,
    matches: Vec<usize>,
}

/// The most recent search the user issued, used to repeat with
/// `n`/`N`, `/<enter>`, or `?<enter>`.  Lives on [`App`] so it survives
/// switching tabs and filter edits — when the user repeats a search,
/// any tab without [`TabSearch`] re-derives one from this pattern.
#[derive(Clone)]
struct LastSearch {
    pattern: String,
    direction: SearchDirection,
}

/// One independent view: name, the rows produced by querying the engine
/// with the host stream's filter, and the scroll offset within those
/// rows.
///
/// Reads the flat materialization (events, formatted lines, the two
/// line/event translation tables, parse stats) through
/// [`Self::materialized`].  For [`TabKind::Stream`] tabs backed by a
/// streamview, the streamview owns and maintains the cache; for
/// summary tabs and the test-fixture path, [`Self::standalone_materialized`]
/// carries an owned copy.
///
/// The active filter is owned by the [`LogStream`] this tab is viewing
/// (looked up in [`Session::streams`] by `stream`); two tabs that target
/// the same stream therefore share a filter — that's the model that
/// makes "open a bookmark whose stream is already shown in some tab"
/// give the user a consistent view.
struct Tab {
    name: String,
    /// Identifier of the [`LogStream`] this tab views.  The stream owns
    /// the filter and other persisted configuration; the display tab
    /// holds the transient render state.
    stream: LogStreamId,
    /// What this tab displays.  [`TabKind::Stream`] is the regular log
    /// view; [`TabKind::Summary`] renders a field/time histogram.
    kind: TabKind,
    /// For [`TabKind::Stream`]: the lazy windowed source.  Slides as
    /// the user scrolls past the window's edges; survives filter
    /// changes (resets to the top) and `show_extras` toggles
    /// (reformats in place).  `None` for [`TabKind::Summary`] (which
    /// keeps the existing full-pass render) and for test-only tabs
    /// constructed via [`App::with_rows`] / [`App::with_events`] that
    /// bypass the engine.
    viewport: Option<Viewport>,
    /// Owned [`Materialized`] used by tabs without a streamview:
    /// [`TabKind::Summary`] (filled by [`SummaryOp::finalize`]) and
    /// test fixtures.  When `streamview` is `Some`, this field is
    /// unused — [`Self::materialized`] dispatches through to
    /// [`StreamView::materialized`] instead.
    standalone_materialized: Materialized,
    /// Index of the *display line* at the top of the viewport.  The
    /// viewport scrolls in line steps so users can see (and search) the
    /// extra-field rows independently from their headers.
    viewport_top: LineIdx,
    /// Active highlighted search, if any.  Match indices are line
    /// indices into the materialization's `formatted`; cleared when
    /// the rows are re-queried (filter change), because the indices
    /// would otherwise dangle.
    search: Option<TabSearch>,
    /// When `Some`, select mode is active.  The contained value carries
    /// the *event* (record) currently highlighted and the action (`x`
    /// exclude, `X` include, `b` bookmark) the Enter key will commit.
    /// Selection sits at record granularity, not display-line, because
    /// the actions all want a single record (build a `msg` predicate,
    /// pin a bookmark to a position).  Cleared whenever the rows are
    /// re-queried so the index can't dangle.
    select: Option<Selection>,
}

/// What committing (hitting enter) in the current selection mode will do
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionAction {
    /// Append to the current filter a predicate that excludes all records whose
    /// message matches the selected record
    Exclude,
    /// Append to the current filter a predicate that includes all records whose
    /// message matches the selected record
    Include,
    /// Create a bookmark pointing at the selected record
    Bookmark,
}

/// State of an in-progress `x`/`X`/`b` selection.
///
/// `event_idx` is an index into [`Tab::events`] (i.e., a record, not a
/// display line).  `action` is what Enter will do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    event_idx: EventIdx,
    action: SelectionAction,
}

impl Tab {
    #[allow(clippy::too_many_arguments)]
    fn new(
        name: String,
        kind: TabKind,
        engine: &Engine,
        stream: LogStreamId,
        filter: &Filter,
        opts: RenderOpts,
    ) -> Self {
        let (viewport, standalone_materialized) = match kind {
            TabKind::Stream => {
                let view = build_viewport(
                    engine,
                    filter,
                    opts,
                    INITIAL_VIEWPORT_HEIGHT,
                );
                (Some(view), Materialized::default())
            }
            // Summary tabs defer their build to a [`LongOp`] driven by
            // the main loop.  Construction installs the placeholder
            // ("Computing summary…") into the standalone slot; the
            // App spins up a [`SummaryOp`] right after pushing the
            // tab so the progress bar shows up on the next frame.
            TabKind::Summary => (None, summary_placeholder_rows()),
        };
        Self {
            name,
            stream,
            kind,
            viewport,
            standalone_materialized,
            viewport_top: LineIdx::ZERO,
            search: None,
            select: None,
        }
    }

    /// Returns the flat materialization of this tab's current content.
    /// Dispatches through to [`StreamView::materialized`] when the tab
    /// is backed by a streamview, or to the owned
    /// [`Self::standalone_materialized`] otherwise (summary tabs and
    /// test fixtures).
    fn materialized(&self) -> &Materialized {
        match self.viewport.as_ref() {
            Some(view) => view.materialized(),
            None => &self.standalone_materialized,
        }
    }

    /// Convenience accessors for the materialization's flat vectors.
    /// Read sites prefer these to spelling out `tab.materialized().X`
    /// every time.
    fn events(&self) -> &[Row] {
        &self.materialized().events
    }

    fn formatted(&self) -> &[String] {
        &self.materialized().formatted
    }

    fn event_for_line(&self) -> &[EventIdx] {
        &self.materialized().event_for_line
    }

    fn first_line_for_event(&self) -> &[LineIdx] {
        &self.materialized().first_line_for_event
    }

    fn parse_stats(&self) -> &ParseStats {
        &self.materialized().parse_stats
    }

    /// Re-renders the host stream like [`Self::refresh`], but keeps the
    /// viewport pinned to the *record* that was at the top before — used
    /// when only the rendering changed (e.g. toggling `show_extras` or
    /// `show_date`), where the underlying events are the same and
    /// resetting to the top would lose the user's place.  Search is
    /// cleared because match indices are line-indexed and lines may have
    /// moved (when `show_extras` toggled); selection is preserved
    /// because it sits on a record index, which is still valid.
    fn rerender(&mut self, engine: &Engine, filter: &Filter, opts: RenderOpts) {
        // Summary tabs don't honor RenderOpts — their histogram is
        // built from raw event counts, not from the per-record render
        // path — so a `rerender` triggered by `F`/`D`/field-display
        // toggles is a no-op here, preserving the already-built
        // histogram instead of throwing it away and rebuilding.
        if self.kind == TabKind::Summary {
            return;
        }
        let anchor_event =
            self.event_for_line().get(self.viewport_top.get()).copied();
        if let Some(view) = self.viewport.as_mut() {
            view.set_render_options(opts);
        } else {
            self.viewport = Some(build_viewport(
                engine,
                filter,
                opts,
                INITIAL_VIEWPORT_HEIGHT,
            ));
            self.standalone_materialized = Materialized::default();
        }
        self.viewport_top = anchor_event
            .and_then(|i| self.first_line_for_event().get(i.get()).copied())
            .unwrap_or(LineIdx::ZERO);
        self.search = None;
    }

    /// Returns the precomputed match indices over `formatted` for tabs
    /// without a [`StreamView`]; an empty vec for streamview tabs
    /// (which navigate via [`StreamView::search_step`] and don't need
    /// a precomputed index).
    fn match_indices(&self, regex: &Regex) -> Vec<usize> {
        if self.viewport.is_some() {
            Vec::new()
        } else {
            compute_matches(self.formatted(), regex)
        }
    }

    // XXX-dap this comment seems wrong
    /// Copies the streamview's current window into the materialized
    /// `events`/`formatted`/index vectors and clamps `viewport_top` to
    /// the streamview's anchor.  Caller must have just driven a
    /// streamview operation that left the anchor on the desired
    /// record/line.  No-op for tabs without a [`StreamView`].
    fn resync_from_streamview(
        &mut self,
        viewport_height: u16,
        viewport_width: u16,
    ) {
        let Some(view) = self.viewport.as_ref() else {
            return;
        };

        let anchor = view.anchor_flat_line();
        let max = self.max_top(viewport_height, viewport_width);
        self.viewport_top = anchor.min(max);
        // When the streamview's anchor sat past `max_top` (e.g. after
        // `seek_to_end` leaves it on the very last line, or after a
        // search lands near the buffer's tail), sync the anchor back
        // to `max_top` so the next backward scroll moves the visible
        // viewport on the first keystroke.  Without this sync the
        // anchor and `viewport_top` would drift apart and `k`
        // keystrokes would shuffle the anchor through the (clamped)
        // viewport until it dropped below `max_top`, looking to the
        // user like navigation had stopped.
        if anchor > max
            && let Some(view) = self.viewport.as_mut()
        {
            view.set_anchor_to_flat_line(max);
        }
    }

    /// Last *display line* index belonging to record `event_idx`,
    /// inclusive.  A header-only record has its first and last on the
    /// same line.
    fn last_line_for_event(&self, event_idx: EventIdx) -> LineIdx {
        let next_first = self
            .first_line_for_event()
            .get(event_idx.get() + 1)
            .copied()
            .unwrap_or(LineIdx(self.formatted().len()));
        // `next_first` is exclusive; the last line for this event is
        // one before it.  Records always contribute at least one line
        // so the subtraction never underflows.
        next_first.saturating_sub(1)
    }

    /// Largest valid `viewport_top`: the smallest logical-line index
    /// whose tail still fits in `viewport_height` visual rows at
    /// `viewport_width` columns.
    ///
    /// Walks backward from the last formatted line, accumulating each
    /// line's wrapped row count, and returns the index of the first
    /// line that would overflow.  Capped at `formatted.len() - 1` so
    /// that even a single oversized line (whose wrap exceeds the
    /// viewport on its own) stays reachable rather than scrolling off
    /// the top.  Returns `0` when the entire buffer fits.  With
    /// `viewport_width == 0` (degenerate terminal, or pre-render
    /// fixtures), `visual_rows_for` collapses to one row per line and
    /// the result matches the original `formatted.len() -
    /// viewport_height` formula.
    fn max_top(&self, viewport_height: u16, viewport_width: u16) -> LineIdx {
        if self.formatted().is_empty() {
            return LineIdx::ZERO;
        }
        let viewport_height = viewport_height as usize;
        let last_idx = self.formatted().len() - 1;
        let mut visual_used: usize = 0;
        for i in (0..=last_idx).rev() {
            let rows = visual_rows_for(&self.formatted()[i], viewport_width);
            visual_used = visual_used.saturating_add(rows);
            if visual_used > viewport_height {
                return LineIdx((i + 1).min(last_idx));
            }
        }
        LineIdx::ZERO
    }

    /// Scrolls `n` display lines forward.  For streamview-backed tabs
    /// this drives [`StreamView::scroll_lines`] so the lazy window can
    /// extend past its initial fetch — without that path scrolling
    /// would clamp at the cached records' edge and the user would never
    /// see anything beyond the first batch.  Tabs without a streamview
    /// (test fixtures and Summary tabs) keep the simple bump-and-clamp
    /// path against the precomputed `formatted` vector.
    fn scroll_down(
        &mut self,
        n: usize,
        viewport_height: u16,
        viewport_width: u16,
    ) {
        if let Some(view) = self.viewport.as_mut() {
            view.scroll_lines(n as isize);
            self.resync_from_streamview(viewport_height, viewport_width);
        } else {
            let max = self.max_top(viewport_height, viewport_width);
            self.viewport_top = (self.viewport_top + n).min(max);
        }
    }

    /// Symmetric to [`Self::scroll_down`].
    fn scroll_up(
        &mut self,
        n: usize,
        viewport_height: u16,
        viewport_width: u16,
    ) {
        if let Some(view) = self.viewport.as_mut() {
            view.scroll_lines(-(n as isize));
            self.resync_from_streamview(viewport_height, viewport_width);
        } else {
            self.viewport_top = self.viewport_top.saturating_sub(n);
        }
    }

    /// Moves the select-mode highlight by `delta` *records* (positive
    /// == later) and scrolls the viewport just enough to keep the new
    /// selection visible.  No-op if select mode is not active or if
    /// there are no records.
    ///
    /// When the newly-selected record's display lines extend past the
    /// viewport bottom we pin the record's *last* line to the bottom
    /// (matching less-style minimal scrolling for single-line records);
    /// records taller than the viewport pin their first line to the top
    /// instead, since clipping the header is more confusing than
    /// clipping the trailing extras.
    fn move_selection(&mut self, delta: isize, viewport_height: u16) {
        let Some(sel) = self.select else {
            return;
        };
        if self.events().is_empty() {
            return;
        }
        let last = self.events().len() - 1;
        let new_idx = EventIdx(
            (sel.event_idx.get() as isize + delta).clamp(0, last as isize)
                as usize,
        );
        self.select = Some(Selection { event_idx: new_idx, ..sel });
        let first = self.first_line_for_event()[new_idx.get()];
        let last_line = self.last_line_for_event(new_idx);
        let height = viewport_height as usize;
        if first < self.viewport_top {
            self.viewport_top = first;
        } else if height > 0
            && last_line.get() >= self.viewport_top.get() + height
        {
            let event_height = last_line - first + 1;
            self.viewport_top = if event_height >= height {
                first
            } else {
                last_line.saturating_sub(height - 1)
            };
        }
    }

    /// Record index of the closest event to `viewport_top` in the
    /// requested direction.  Falls back to the opposite direction so a
    /// viewport parked on an error row at one end of the file still gets
    /// an anchor; returns `None` only when there are no parsed events
    /// at all.  Used by [`Self::advance_time`] to decide what timestamp
    /// to add the step to.
    fn time_anchor_idx(&self, prefer: Direction) -> Option<EventIdx> {
        // Translate the line-indexed viewport_top to its enclosing
        // record so the search range matches the user's visual
        // position.  When the viewport is parked past the last line
        // (only possible if `formatted` is empty, in which case events
        // is too) `event_for_line` would index out of range — check
        // length first.
        let pivot = if self.viewport_top.get() < self.event_for_line().len() {
            self.event_for_line()[self.viewport_top.get()].get()
        } else {
            self.events().len()
        };
        let forward =
            self.events().iter().enumerate().skip(pivot).find_map(|(i, e)| {
                matches!(e, Row::Event(_)).then_some(EventIdx(i))
            });
        let backward_take = pivot.saturating_add(1).min(self.events().len());
        let backward = self
            .events()
            .iter()
            .enumerate()
            .take(backward_take)
            .rev()
            .find_map(|(i, e)| {
                matches!(e, Row::Event(_)).then_some(EventIdx(i))
            });
        match prefer {
            Direction::Forward => forward.or(backward),
            Direction::Backward => backward.or(forward),
        }
    }

    /// Moves `viewport_top` forward (positive `delta`) or backward
    /// (negative `delta`) by approximately `delta` of wall-clock time.
    ///
    /// Concretely: pick an anchor event near `viewport_top` (in the
    /// chosen direction, with fallback), then jump to the first event
    /// at or past `anchor.time + delta` in the same direction.  If no
    /// event satisfies the criterion the viewport snaps to the
    /// corresponding end of the buffer — that's "as close as we can
    /// get" rather than silently doing nothing, which would be
    /// confusing when the user expects motion.  No-op when the tab
    /// holds no parsed events.
    fn advance_time(
        &mut self,
        direction: Direction,
        delta: chrono::Duration,
        viewport_height: u16,
        viewport_width: u16,
    ) {
        if let Some(view) = self.viewport.as_mut() {
            // Lazy path: walks the engine's stepper, fetching only as
            // far as needed to land on the target time.
            view.start_seek_by_time(direction, delta);
            self.resync_from_streamview(viewport_height, viewport_width);
            return;
        }
        // Fallback for tabs without a StreamView (test fixtures and
        // Summary tabs): scan the materialized events vector.
        let dir = if delta.num_milliseconds() > 0 {
            Direction::Forward
        } else {
            Direction::Backward
        };
        let Some(anchor_idx) = self.time_anchor_idx(dir) else {
            return;
        };
        let Row::Event(anchor) = &self.events()[anchor_idx.get()] else {
            unreachable!("time_anchor_idx returns indices of real events");
        };
        let anchor_time = anchor.event.time;
        let target = anchor_time + delta;
        let max = self.max_top(viewport_height, viewport_width);
        let new_event = match dir {
            Direction::Forward => self
                .events()
                .iter()
                .enumerate()
                .skip(anchor_idx.get())
                .find_map(|(i, e)| match e {
                    Row::Event(ee) if ee.event.time >= target => {
                        Some(EventIdx(i))
                    }
                    _ => None,
                }),
            Direction::Backward => self
                .events()
                .iter()
                .enumerate()
                .take(anchor_idx.get() + 1)
                .rev()
                .find_map(|(i, e)| match e {
                    Row::Event(ee) if ee.event.time <= target => {
                        Some(EventIdx(i))
                    }
                    _ => None,
                }),
        };
        let new_top = match new_event {
            Some(idx) => self.first_line_for_event()[idx.get()],
            None if dir == Direction::Forward => max,
            None => LineIdx::ZERO,
        };
        self.viewport_top = new_top.min(max);
    }
}

/// Time-based navigation step sizes, smallest to largest.  `<` and `>`
/// advance the active tab by the currently selected step; `=` (or `+`)
/// moves to the next-larger step and `-` to the next-smaller, both
/// clamped at the ends.  Stored as (display label, milliseconds) pairs
/// so the footer can show the user-visible label without re-deriving
/// it.
const TIME_STEPS: &[(&str, i64)] = &[
    ("100ms", 100),
    ("1s", 1_000),
    ("5s", 5_000),
    ("10s", 10_000),
    ("30s", 30_000),
    ("1m", 60_000),
    ("5m", 5 * 60_000),
    ("10m", 10 * 60_000),
    ("30m", 30 * 60_000),
    ("60m", 60 * 60_000),
    ("6h", 6 * 60 * 60_000),
    ("12h", 12 * 60 * 60_000),
    ("1d", 24 * 60 * 60_000),
];

/// Default starting step.  "1m" is the median of the table and a
/// reasonable opening gambit for triaging multi-minute incidents
/// without zooming all the way out to hours.
const DEFAULT_TIME_STEP_IDX: usize = 5;

/// All TUI state.  Pure with respect to I/O so [`App::handle_key`] can
/// be unit-tested by feeding synthetic key events.
///
/// `tabs` holds the display state for currently-open tabs; `session`
/// holds everything that survives a restart (open streams' filters, the
/// user's bookmarks, eventually saved cursors and named filter sets).
/// The two stay in sync: every `tabs[i]` references a `LogStream` that
/// lives in `session.streams`.
struct App {
    engine: Engine,
    /// Persistent session state.  When non-empty bookmarks live here a
    /// synthetic Bookmarks tab is rendered after the regular tabs.
    session: Session,
    /// Open tabs, in display order.  Invariant: never empty (closing
    /// the last tab pushes a fresh one to maintain this).
    tabs: Vec<Tab>,
    /// Index into the *virtual* tab list of the currently visible
    /// pane.  When this equals `tabs.len()` the synthetic Bookmarks
    /// tab is active; otherwise it indexes into `tabs`.
    active: usize,
    /// Monotonically-increasing counter used to name new tabs.  Never
    /// reused — closing "Tab 2" leaves the next new tab named "Tab 4"
    /// rather than overlapping with an existing "Tab 3".
    next_tab_number: usize,
    /// Updated on each [`render`] call from the actual frame size.
    viewport_height: u16,
    /// Width of the content area in columns.  Updated on each
    /// [`render`] call from the actual frame size.  Used together with
    /// [`Self::viewport_height`] to compute visual row counts for
    /// wrapped lines (so `max_top` and the footer's range stay
    /// accurate when long lines span multiple terminal rows).
    viewport_width: u16,
    quit: bool,
    /// When `Some`, this dialog is open and intercepts all keys.
    dialog: Option<Dialog>,
    /// Most recent search pattern + direction.  Outlives any
    /// individual tab's [`TabSearch`] so `n` / `N` / empty repeats
    /// still work after switching tabs or editing the filter (which
    /// clears `tab.search`).
    last_search: Option<LastSearch>,
    /// Index into [`TIME_STEPS`] of the current step used by `<` / `>`.
    /// App-level (rather than per-tab) so the user sets it once and it
    /// applies wherever they navigate next, mirroring how `last_search`
    /// is shared across tabs.
    time_step_idx: usize,
    /// Currently highlighted bookmark id when the Bookmarks tab is
    /// active.  Cleared (set to `None`) when the bookmark it pointed at
    /// is deleted or no bookmarks remain.
    bookmark_cursor: Option<BookmarkId>,
    /// One-shot status text shown in the footer for one render — used
    /// to tell the user "the bookmarked entry is hidden by the active
    /// filter" or "the bookmarked entry is gone" after a navigation.
    /// Cleared by the next user keystroke.
    notice: Option<String>,
    /// On-disk store backing this session, or `None` for a transient
    /// session that should never be written to disk.  Test
    /// constructors leave this `None`.
    store: Option<SessionStore>,
    /// Save-cadence bookkeeping for session-affecting mutations.
    policy: SavePolicy,
    /// When true, draw a developer-oriented row above the user status
    /// line showing engine fetch progress (`X records (Y) fetched in
    /// Zs ...`).  Toggled with `p` and not persisted across sessions:
    /// it's a debugging affordance, not part of the user's view state.
    show_fetch_stats: bool,
}

impl App {
    /// Convenience constructor for tests that don't care about the
    /// session or persistence.  Production code goes through
    /// [`Self::new_with_session`] so a previously-saved session and
    /// the on-disk store are honored.
    #[cfg(test)]
    fn new_for_tests(engine: Engine) -> Self {
        Self::new_with_session(
            engine,
            Session::new(),
            None,
            SavePolicy::new(SavePolicy::DEFAULT_DEBOUNCE),
        )
    }

    /// Constructs an [`App`] reusing a previously-loaded [`Session`].
    /// Restores the user's prior tab set, their bookmarks, and the
    /// streams those reference.  Each persisted [`seer::Tab`] becomes
    /// a runtime tab backed by its target [`LogStream`]'s filter, and
    /// the saved cursor (if any) is fed to
    /// [`StreamView::seek_to_cursor`] so the viewport lands back on
    /// the same record.  When the session has no tabs (a fresh
    /// session, or one saved before tab persistence landed), the App
    /// falls back to a single unfiltered Stream tab — the invariant
    /// is that `tabs` is never empty.
    ///
    /// `store` is `None` for a transient session that should not be
    /// written to disk; `policy` is the save-cadence tracker
    /// (initialized by the caller so the freshly-saved-at-startup
    /// timestamp is already recorded).
    fn new_with_session(
        engine: Engine,
        session: Session,
        store: Option<SessionStore>,
        policy: SavePolicy,
    ) -> Self {
        let mut a = Self {
            engine,
            session,
            tabs: Vec::new(),
            active: 0,
            next_tab_number: 1,
            viewport_height: 0,
            viewport_width: 0,
            quit: false,
            dialog: None,
            last_search: None,
            time_step_idx: DEFAULT_TIME_STEP_IDX,
            bookmark_cursor: None,
            notice: None,
            show_fetch_stats: false,
            store,
            policy,
        };
        a.restore_tabs_or_default();
        a
    }

    /// Rebuilds [`Self::tabs`] from [`Session::tabs`] on resume.  When
    /// the session has at least one persisted tab whose target stream
    /// still exists, one runtime tab is built per entry; otherwise the
    /// App falls back to a single fresh Stream tab so the
    /// "tabs is never empty" invariant holds.  Persisted Summary tabs
    /// re-enqueue their histogram build via the standard long-op path,
    /// matching the behavior the user gets when they open a Summary
    /// tab interactively.
    fn restore_tabs_or_default(&mut self) {
        // Take the persisted list so it doesn't appear "doubled up"
        // once `push_tab_restored` starts repopulating it on save.
        let persisted = std::mem::take(&mut self.session.tabs);

        // First pass: bump next_tab_number past every "Tab N" /
        // "Summary N" name we'll be restoring, so later user-driven
        // pushes don't collide with names already in use.  User-renamed
        // tabs don't match the `Word N` shape, so `parse_tab_number`
        // returns `None` and they don't perturb the counter.
        for ptab in &persisted {
            if let Some(n) = parse_tab_number(&ptab.name) {
                self.next_tab_number = self.next_tab_number.max(n + 1);
            }
        }

        let mut restored_any = false;
        for ptab in persisted {
            let Some(stream) = self.session.streams.get(&ptab.stream) else {
                // Tab pointed at a stream that's gone — skip it; the
                // user will see one fewer tab but no broken pointer.
                continue;
            };
            let name = ptab.name.clone();
            let filter = stream.filter.clone();
            let opts = stream.render_opts();
            let mut tab = Tab::new(
                name,
                ptab.kind,
                &self.engine,
                ptab.stream,
                &filter,
                opts,
            );
            // Restore the scroll position when we have one and the
            // tab carries a streamview to seek with.  Summary tabs
            // have no streamview, so cursor restore is a no-op for
            // them — the histogram build will resume from scratch.
            //
            // Passing zero for viewport width/height makes `max_top`
            // collapse to "the last line", so `resync` lands
            // `viewport_top` on the streamview anchor; the first
            // render frame supplies the real terminal size and the
            // viewport clamps down if needed.
            if let (Some(cursor), Some(view)) =
                (ptab.cursor.clone(), tab.viewport.as_mut())
            {
                view.start_seek_to_cursor(&self.engine, &cursor);
                tab.resync_from_streamview(0, 0);
            }
            let tab_idx = TabIdx(self.tabs.len());
            self.tabs.push(tab);
            // Summary tabs need their long-op build queued, same as
            // when the user opens one interactively.
            if ptab.kind == TabKind::Summary {
                self.enqueue_summary_build(tab_idx, filter);
            }
            restored_any = true;
        }

        if restored_any {
            self.active = 0;
        } else {
            // No usable persisted tabs — preserve the legacy startup
            // shape with one fresh Stream tab.
            self.push_tab(TabKind::Stream, Filter::default());
        }
    }

    /// Persists the current [`Session`] to disk through the attached
    /// [`SessionStore`], if any, and records the flush with the save
    /// policy.  Returns `Ok(())` (with no I/O) when no store is
    /// attached — a transient session quietly skips persistence.
    ///
    /// On failure the policy's `dirty` bit is left alone (it stays
    /// set if the caller had just recorded a mutation) so the next
    /// opportunity tries again.  Callers that want to surface the
    /// error to the user typically do so via [`Self::notice`].
    fn try_save_now(&mut self) -> Result<(), StoreError> {
        self.sync_tabs_to_session();
        if let Some(store) = self.store.as_ref() {
            store.save(self.session.id, &self.session)?;
            self.policy.mark_saved(Instant::now());
        }
        Ok(())
    }

    /// Mirrors the runtime [`Self::tabs`] list into [`Session::tabs`] so
    /// a subsequent `store.save` captures the user's open tabs.  Each
    /// runtime tab contributes its stream id, kind, and (for stream
    /// tabs) the [`Cursor`] at the viewport anchor — fed back into
    /// [`StreamView::seek_to_cursor`] on resume to land on the same
    /// record.  Summary tabs have no streamview, so their `cursor` is
    /// always `None` — the histogram is rebuilt from the filter, not
    /// scrolled to a position.
    fn sync_tabs_to_session(&mut self) {
        let tabs: Vec<seer::Tab> = self
            .tabs
            .iter()
            .map(|t| seer::Tab {
                name: t.name.clone(),
                stream: t.stream,
                kind: t.kind,
                cursor: t.viewport.as_ref().and_then(|v| v.cursor_at_anchor()),
            })
            .collect();
        self.session.tabs = tabs;
    }

    /// Records that an inline-cadence mutation just happened and
    /// flushes the session to disk right away.  Save failures are
    /// surfaced through [`Self::notice`] so the user sees them in
    /// the footer; the dirty bit stays set so the next save
    /// opportunity (another mutation, the debounce tick, or exit)
    /// retries.
    ///
    /// Call this at the *end* of every low-cadence mutation method
    /// (bookmark create / delete, tab open / close, filter change,
    /// field show / hide).  Helper methods that several user
    /// gestures share — e.g. [`Self::rerender_after_stream_mutation`]
    /// — must not call this themselves; the user-gesture method on
    /// the outside is the right level.
    fn save_after_inline_mutation(&mut self) {
        self.policy.record(Cadence::Inline);
        if let Err(e) = self.try_save_now() {
            self.notice = Some(format!("session save failed: {e}"));
        }
    }

    /// Polled once per event-loop iteration.  Flushes the session if
    /// pending debounced changes have aged past the policy's window;
    /// otherwise a no-op.  Failures are surfaced via
    /// [`Self::notice`], the dirty bit stays set, and the next tick
    /// retries.
    fn flush_if_due(&mut self) {
        if self.policy.due(Instant::now())
            && let Err(e) = self.try_save_now()
        {
            self.notice = Some(format!("session save failed: {e}"));
        }
    }

    /// Pushes a new tab backed by a fresh [`LogStream`] with the given
    /// filter.  Does *not* open the filter dialog — callers that want
    /// that (e.g. Ctrl-T) do it explicitly after.  Switches focus to
    /// the new tab.  `kind` decides whether the new tab is a regular
    /// stream view or a Summary histogram.
    fn push_tab(&mut self, kind: TabKind, filter: Filter) {
        let name = match kind {
            TabKind::Stream => format!("Tab {}", self.next_tab_number),
            TabKind::Summary => format!("Summary {}", self.next_tab_number),
        };
        self.next_tab_number += 1;
        let mut stream = LogStream::new(name.clone());
        stream.filter = filter;
        let stream_id = stream.id;
        let pushed_filter = stream.filter.clone();
        let render_opts = stream.render_opts();
        self.session
            .streams
            .insert_unique(stream)
            .expect("freshly-minted LogStreamId is unique");
        let tab = Tab::new(
            name,
            kind,
            &self.engine,
            stream_id,
            &pushed_filter,
            render_opts,
        );
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        // Summary tabs are placeholder-only at construction; the
        // build runs as a [`LongOp`] so the user sees a progress bar
        // while the histogram is computed.
        if kind == TabKind::Summary {
            self.enqueue_summary_build(
                TabIdx(self.tabs.len() - 1),
                pushed_filter,
            );
        }
        self.save_after_inline_mutation();
    }

    /// Queues a Summary rebuild for the tab at `tab_idx`.  Starts the
    /// build immediately when no [`LongOp`] is in flight; otherwise
    /// the request waits in [`Self::pending_summary_builds`] and is
    /// dispatched when the active op finalizes.  Replacing an
    /// already-pending build for the same tab keeps the queue
    /// monotonic — only the most recent filter for a given tab is
    /// honored, since older requests would compute against stale data.
    fn enqueue_summary_build(&mut self, _tab_idx: TabIdx, _filter: Filter) {
        // Drop any earlier pending request for the same tab — a fresh
        // request supersedes it.
        // self.pending_summary_builds.retain(|(idx, _)| *idx != tab_idx);
        // XXX-dap need to figure out how builds fit into this
        // if self.long_op.is_none() {
        //     self.start_summary_build(tab_idx, filter);
        // } else {
        //     self.pending_summary_builds.push_back((tab_idx, filter));
        // }
    }

    /// Resets the destination tab to the placeholder shape and
    /// installs a fresh [`SummaryOp`] in [`Self::long_op`].  Caller is
    /// responsible for ensuring no other [`LongOp`] is in flight.
    fn start_summary_build(&mut self, tab_idx: TabIdx, _filter: Filter) {
        // XXX-dap
        // debug_assert!(self.long_op.is_none());
        if let Some(tab) = self.tab_mut(tab_idx) {
            tab.standalone_materialized = summary_placeholder_rows();
            tab.viewport_top = LineIdx::ZERO;
            tab.search = None;
        }
        // let total_bytes = self.engine.filtered_total_bytes(&filter);
        // XXX-dap need to figure out how summaries fit into this
        // self.long_op = Some(LongOp::BuildSummary(Box::new(SummaryOp::new(
        //     tab_idx,
        //     filter,
        //     total_bytes,
        // ))));
    }

    fn is_busy(&self) -> bool {
        let tab = self.active_tab();

        // XXX-dap summary tabs will need to do something
        let Some(viewport) = &tab.viewport else {
            return false;
        };

        // XXX-dap this isn't quite right.  We have two different bits to
        // return: whether we're interruptible right now and whether the caller
        // should call do_work().  Right now we implement the latter.
        match viewport.status() {
            ViewportStatus::Idle => false,
            ViewportStatus::Populating | ViewportStatus::Seeking(_) => true,
        }
    }

    fn do_work(&mut self) {
        let tab = self.active_tab_mut();
        let Some(viewport) = &mut tab.viewport else {
            return;
        };

        viewport.seek_work();
        viewport.populate_work();
    }

    fn interrupt(&mut self) {
        let tab = self.active_tab_mut();
        let Some(viewport) = &mut tab.viewport else {
            return;
        };

        viewport.seek_interrupt();
    }

    //    XXX-dap
    //    /// Drives the active [`LongOp`] forward by one chunk.  No-op when
    //    /// no op is in flight.  The op owns its own borrowing rules: a
    //    /// summary build talks only to the engine, while a search step
    //    /// also needs the destination tab's [`StreamView`].
    //    ///
    //    /// Returns `true` if the op finished (or was already finished) on
    //    /// this call — the caller can use that as a signal to schedule a
    //    /// final repaint, but it is not required: `long_op` is also
    //    /// cleared as a side effect.
    //    fn advance_long_op(&mut self) -> bool {
    //        let Some(mut op) = self.long_op.take() else { return false };
    //        let h = self.viewport_height;
    //        let done = match &mut op {
    //            LongOp::BuildSummary(s) => s.advance(&self.engine),
    //            LongOp::Search(s) => self.advance_search_op(s, h),
    //            LongOp::Seek(s) => self.advance_seek_op(s, h),
    //        };
    //        if done {
    //            self.finalize_long_op(op);
    //            true
    //        } else {
    //            self.long_op = Some(op);
    //            false
    //        }
    //    }
    //
    //    /// Drives one tick of an in-progress seek.  Each call to
    //    /// [`StreamView::ensure_window_step`] runs at most one bounded
    //    /// scan (capped at `LONG_OP_RECORDS_TO_SCAN_PER_FILL` records examined),
    //    /// so the wall time per tick is predictable even when the
    //    /// active filter rejects almost everything.  Returns `true` once
    //    /// the window-fill reaches its target or hits EOF in the fill
    //    /// direction.
    //    fn advance_seek_op(
    //        &mut self,
    //        s: &mut SeekOp,
    //        viewport_height: u16,
    //    ) -> bool {
    //        let tab_idx = s.tab_idx;
    //        let App { tabs, engine, .. } = self;
    //        let Some(view) =
    //            tabs.get_mut(tab_idx.get()).and_then(|t| t.streamview.as_mut())
    //        else {
    //            // Tab vanished (closed under us) or never had a
    //            // streamview to begin with.  Nothing left to fill.
    //            s.complete = true;
    //            return true;
    //        };
    //        let completed = matches!(
    //            view.ensure_window_step(engine, viewport_height),
    //            WindowFillStatus::Done
    //        );
    //        let stats = view.parse_stats();
    //        s.bytes_done =
    //            stats.walked_bytes.saturating_sub(s.walked_bytes_at_start);
    //        s.records = stats.records.saturating_sub(s.records_at_start);
    //        if completed {
    //            s.complete = true;
    //        }
    //        completed
    //    }

    //    /// Drives one chunk of an in-progress search.  Borrows the
    //    /// destination tab's [`StreamView`] mutably to call
    //    /// [`StreamView::search_step_with_budget`], then snapshots the
    //    /// streamview's running parse stats into the op so the progress
    //    /// bar can read them later without re-borrowing.  Returns `true`
    //    /// when the search is terminal (Found / NotFound / Cancelled),
    //    /// `false` when the streamview wants to be called again (budget
    //    /// exhausted).
    //    fn advance_search_op(
    //        &mut self,
    //        s: &mut SearchOp,
    //        viewport_height: u16,
    //    ) -> bool {
    //        let tab_idx = s.tab_idx;
    //        // Split-borrow `self` so the streamview (in `tabs[tab_idx]`)
    //        // can be borrowed mutably while `engine` is borrowed shared
    //        // for the same call.  Field-level destructure makes the two
    //        // borrows independent in the borrow checker's eyes.
    //        let App { tabs, engine, .. } = self;
    //        let Some(view) =
    //            tabs.get_mut(tab_idx.get()).and_then(|t| t.streamview.as_mut())
    //        else {
    //            // The tab disappeared (closed under us) or never had a
    //            // streamview to begin with.  Treat as terminal — nothing
    //            // to scan.
    //            s.outcome = Some(SearchOutcome::NotFound);
    //            return true;
    //        };
    //        let outcome = view.search_step_with_budget(
    //            engine,
    //            &s.regex,
    //            s.direction,
    //            s.anchor,
    //            viewport_height,
    //            LONG_OP_CHUNK_RECORDS,
    //            &mut || false,
    //        );
    //        // After the first chunk subsequent chunks must not skip the
    //        // anchor again — the streamview's saved resume point handles
    //        // "where did we leave off."
    //        s.anchor = SearchAnchor::Include;
    //        let stats = view.parse_stats();
    //        s.bytes_done =
    //            stats.walked_bytes.saturating_sub(s.walked_bytes_at_start);
    //        s.records = stats.records.saturating_sub(s.records_at_start);
    //        match outcome {
    //            SearchOutcome::Found
    //            | SearchOutcome::NotFound
    //            | SearchOutcome::Cancelled => {
    //                s.outcome = Some(outcome);
    //                true
    //            }
    //            // Auto-resume.  The streamview saved a resume point
    //            // internally; the next chunk picks up where this one
    //            // stopped.
    //            SearchOutcome::BudgetExhausted => false,
    //        }
    //    }

    //     XXX-dap need to replace some of the functionality here
    //     /// Installs the result of a finished [`LongOp`] into the
    //     /// destination tab.  For Summary, swaps in the histogram lines and
    //     /// the final parse stats.  For Search, resyncs the tab's viewport
    //     /// from the streamview's new anchor (when found) and posts a
    //     /// "no match" notice when the scan ran out without one.
    //     fn finalize_long_op(&mut self, op: LongOp) {
    //         match op {
    //             LongOp::BuildSummary(s) => {
    //                 let tab_idx = s.tab_idx;
    //                 let materialized = s.finalize();
    //                 if let Some(tab) = self.tab_mut(tab_idx) {
    //                     tab.standalone_materialized = materialized;
    //                     tab.viewport_top = LineIdx::ZERO;
    //                 }
    //                 // Drain a queued Summary build so a filter change
    //                 // that dirtied multiple Summary tabs progresses
    //                 // through them one after another.
    //                 if let Some((next_idx, next_filter)) =
    //                     self.pending_summary_builds.pop_front()
    //                 {
    //                     self.start_summary_build(next_idx, next_filter);
    //                 }
    //             }
    //             LongOp::Search(s) => {
    //                 let tab_idx = s.tab_idx;
    //                 let outcome = s.outcome.unwrap_or(SearchOutcome::NotFound);
    //                 let h = self.viewport_height;
    //                 let w = self.viewport_width;
    //                 match outcome {
    //                     SearchOutcome::Found => {
    //                         if let Some(tab) = self.tab_mut(tab_idx) {
    //                             tab.resync_from_streamview(h, w);
    //                         }
    //                     }
    //                     SearchOutcome::NotFound => {
    //                         // Match `less`: silently leave the cursor
    //                         // alone.  A notice would be noise on every
    //                         // search-to-end of a large file.
    //                     }
    //                     SearchOutcome::Cancelled => {
    //                         self.notice = Some("search cancelled".to_string());
    //                     }
    //                     SearchOutcome::BudgetExhausted => {
    //                         // Budget exhaustion isn't a terminal outcome
    //                         // for this driver — `advance_long_op` consumes
    //                         // it and re-issues the call.  If we somehow
    //                         // see it here, leave a breadcrumb rather than
    //                         // panicking.
    //                         self.notice = Some(
    //                             "search stopped at budget; press n to resume"
    //                                 .to_string(),
    //                         );
    //                     }
    //                 }
    //             }
    //             LongOp::Seek(s) => {
    //                 self.finalize_seek_op(s);
    //             }
    //         }
    //     }

    //    /// Resolves a finished [`SeekOp`]'s anchor and refreshes the
    //    /// destination tab's materialized rows.  Called from
    //    /// [`Self::finalize_long_op`].
    //    fn finalize_seek_op(&mut self, s: SeekOp) {
    //        let tab_idx = s.tab_idx;
    //        if self.tab(tab_idx).is_none() {
    //            // Tab vanished while the op was running.  Drain any
    //            // queued Summary build so the rest of the filter-rebuild
    //            // chain still makes progress.
    //            self.drain_pending_summary_builds();
    //            return;
    //        }
    //        let h = self.viewport_height;
    //        let w = self.viewport_width;
    //        // Apply the finalize step to the streamview before we resync,
    //        // so the materialized rows reflect the resolved anchor.
    //        if let Some(view) = self.tabs[tab_idx.get()].streamview.as_mut() {
    //            match s.finalize {
    //                SeekFinalize::Front | SeekFinalize::Back => {
    //                    // `ensure_window_step` already resolved PinFront/
    //                    // PinBack on its final tick; nothing more to do.
    //                }
    //                SeekFinalize::FrontOrBackFallback => {
    //                    // The cursor-driven path: if the forward fetch
    //                    // came up empty, try once backward (bounded by the
    //                    // same `ensure_window` target_lines).  Slow with
    //                    // pathological filters, but the simple cases
    //                    // (filter just hides the bookmarked record, not
    //                    // every record before it) finish in one batch.
    //                    if view.is_empty() {
    //                        view.set_anchor_pin_back();
    //                        view.ensure_window(&self.engine, h);
    //                    }
    //                }
    //            }
    //        }
    //        self.tabs[tab_idx.get()].resync_from_streamview(h, w);
    //        // Filter rebuild may have queued a Summary build behind this
    //        // seek (via `apply_filter`'s active-tab-first ordering); kick
    //        // it off now that our long op slot is free.
    //        self.drain_pending_summary_builds();
    //    }

    /// Pops one pending Summary build off the queue and starts it,
    /// installing it as the active long op.  No-op when the queue is
    /// empty or a long op is already running (so the caller can use
    /// this freely after any other long-op finalize without risking a
    /// race).
    fn drain_pending_summary_builds(&mut self) {
        // XXX-dap
        // if self.long_op.is_some() {
        //     return;
        // }
        // if let Some((next_idx, next_filter)) =
        //     self.pending_summary_builds.pop_front()
        // {
        //     self.start_summary_build(next_idx, next_filter);
        // }
    }

    // XXX-dap
    //    /// Cancels the active [`LongOp`], discarding any partial work.
    //    /// Summary builds drop their accumulator (the destination tab is
    //    /// left with its placeholder rows); searches leave the streamview
    //    /// anchor untouched (matching [`SearchOutcome::Cancelled`]
    //    /// semantics).
    //    fn cancel_long_op(&mut self) {
    //        let Some(op) = self.long_op.take() else { return };
    //        match op {
    //            LongOp::BuildSummary(s) => {
    //                let tab_idx = s.tab_idx;
    //                if let Some(tab) = self.tab_mut(tab_idx) {
    //                    tab.standalone_materialized = Materialized {
    //                        events: Vec::new(),
    //                        formatted: vec![
    //                            "summary cancelled — rerun with `f<enter>`"
    //                                .to_string(),
    //                        ],
    //                        event_for_line: vec![EventIdx::ZERO],
    //                        first_line_for_event: vec![LineIdx::ZERO],
    //                        parse_stats: ParseStats::default(),
    //                    };
    //                }
    //                // Cancel any queued Summary builds too — the user
    //                // pressed Ctrl-C to stop, not to skip ahead.
    //                self.pending_summary_builds.clear();
    //            }
    //            LongOp::Search(_) => {
    //                self.notice = Some("search cancelled".to_string());
    //            }
    //            LongOp::Seek(s) => {
    //                // Cancel the seek mid-fetch: keep whatever partial
    //                // window was built so the user doesn't lose their
    //                // place, but resolve the anchor and resync so the
    //                // (partial) records are visible.  A notice tells them
    //                // the op stopped early.
    //                let label = s.label.clone();
    //                self.finalize_seek_op(s);
    //                self.notice = Some(format!("{label} cancelled"));
    //            }
    //        }
    //    }

    // XXX-dap
    ///// Convenience for tests (and code that wants to wait for an op to
    ///// finish): drives [`Self::advance_long_op`] in a loop until no
    ///// long op is in flight.  Caps iterations to avoid an infinite
    ///// loop if some advance ever stops making progress.
    //#[cfg(test)]
    //fn drain_long_op(&mut self) {
    //    for _ in 0..1_000_000 {
    //        if self.long_op.is_none() {
    //            return;
    //        }
    //        self.advance_long_op();
    //    }
    //    panic!("drain_long_op did not converge");
    //}

    /// Pushes a new tab targeting an existing [`LogStream`] (looked up
    /// in `session.streams`).  Used when navigating to a bookmark
    /// whose stream isn't currently shown in any tab — the new tab
    /// inherits the stream's persisted filter and field-visibility
    /// setting.
    fn push_tab_for_existing_stream(&mut self, stream_id: LogStreamId) {
        let name = format!("Tab {}", self.next_tab_number);
        self.next_tab_number += 1;
        let stream = self
            .session
            .streams
            .get(&stream_id)
            .expect("caller verified the stream exists");
        let tab = Tab::new(
            name,
            TabKind::Stream,
            &self.engine,
            stream_id,
            &stream.filter,
            stream.render_opts(),
        );
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.save_after_inline_mutation();
    }

    fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Returns the tab at `idx`, or `None` if the index is out of
    /// range (for example because a long op outlived the tab it was
    /// targeting).
    fn tab(&self, idx: TabIdx) -> Option<&Tab> {
        self.tabs.get(idx.0)
    }

    /// Mutable counterpart to [`Self::tab`].
    fn tab_mut(&mut self, idx: TabIdx) -> Option<&mut Tab> {
        self.tabs.get_mut(idx.0)
    }

    /// Resets the active tab to the end of the merged stream behind a
    /// [`LongOp::Seek`] so a selective filter that has to walk many
    /// on-disk records to find the last `viewport_height` matching
    /// ones doesn't freeze the UI.  Errors from `Source::byte_len()`
    /// (used to compute the end-of-file cursor) surface as a notice;
    /// the prior viewport is unchanged.
    fn seek_active_to_end(&mut self) {
        self.policy.record(Cadence::Debounced);
        let h = self.viewport_height;
        let w = self.viewport_width;
        let active = self.active;
        // Test fixtures (and Summary tabs) without a streamview keep
        // the simple synchronous max_top clamp.  The long-op chunking
        // only helps the engine-backed path.
        let Some(view) = self.tabs[active].viewport.as_mut() else {
            self.tabs[active].viewport_top = self.tabs[active].max_top(h, w);
            return;
        };
        view.start_seek_to_end(&self.engine);
        // Clear the prior tab content so the user sees an empty view
        // with a progress bar (rather than the stale records they
        // were looking at before `G`).
        self.tabs[active].resync_from_streamview(h, w);
    }

    /// Resets the active tab to the start of the merged stream behind
    /// a [`LongOp::Seek`].  Same UX rationale as
    /// [`Self::seek_active_to_end`].
    fn seek_active_to_start(&mut self) {
        self.policy.record(Cadence::Debounced);
        let h = self.viewport_height;
        let w = self.viewport_width;
        let active = self.active;
        let Some(view) = self.tabs[active].viewport.as_mut() else {
            self.tabs[active].viewport_top = LineIdx::ZERO;
            return;
        };
        view.start_seek_to_start(&self.engine);
        self.tabs[active].resync_from_streamview(h, w);
    }

    /// Reseats the active tab on `cursor` behind a [`LongOp::Seek`].
    /// Used by bookmark navigation and (transitively) filter rebuild;
    /// the long-op carries the cursor-fallback semantics that
    /// [`StreamView::seek_to_cursor`] does inline.
    fn seek_active_to_cursor(&mut self, cursor: Cursor) {
        self.policy.record(Cadence::Debounced);
        let h = self.viewport_height;
        let w = self.viewport_width;
        let active = self.active;
        let Some(view) = self.tabs[active].viewport.as_mut() else {
            return;
        };
        view.start_seek_to_cursor(&self.engine, &cursor);
        self.tabs[active].resync_from_streamview(h, w);
    }

    /// True iff the synthetic Bookmarks tab is the currently active
    /// pane.  The Bookmarks tab is implicit (rendered iff there are
    /// any bookmarks, never explicitly opened or closed) and slots in
    /// at index `tabs.len()` in the virtual tab list.
    fn bookmarks_active(&self) -> bool {
        self.has_bookmarks_tab() && self.active == self.tabs.len()
    }

    /// True iff the synthetic Bookmarks tab should be rendered at all
    /// — gated entirely on whether the user has any bookmarks.
    fn has_bookmarks_tab(&self) -> bool {
        self.session.bookmark_count() > 0
    }

    /// Total number of panes in the virtual tab list (regular tabs +
    /// the Bookmarks tab when present).
    fn pane_count(&self) -> usize {
        self.tabs.len() + usize::from(self.has_bookmarks_tab())
    }

    /// Replaces the active stream's filter, re-queries the engine,
    /// and refreshes every tab targeting that stream.  Two tabs
    /// sharing a stream therefore share their filter — that's the
    /// model that lets a bookmark-driven "open in a new tab" carry
    /// the stream's filter forward.
    ///
    /// Each refreshed tab keeps its viewport as close as possible to
    /// where it was: a Cursor is captured from the streamview's
    /// current anchor (when one exists), and the post-refresh
    /// streamview seeks to that cursor.  When the anchored record is
    /// hidden by the new filter, [`StreamView::seek_to_cursor`] slides
    /// to the nearest visible neighbor (forward first, backward as a
    /// fallback) instead of leaving an empty view.  Tabs without a
    /// streamview (test fixtures, Summary tabs) fall back to a
    /// top-of-stream refresh.
    fn apply_filter(&mut self, filter: Filter) {
        let stream_id = self.tabs[self.active].stream;
        let Some(mut stream) = self.session.streams.remove(&stream_id) else {
            return;
        };
        stream.filter = filter;
        let new_filter = stream.filter.clone();
        self.session
            .streams
            .insert_unique(stream)
            .expect("removed-then-reinserted id is unique");

        // Collect indices first so we can call App-level helpers
        // (which need `&mut self`) for Summary rebuilds without
        // holding a `tabs.iter_mut()` borrow alive.
        let affected: Vec<(usize, TabKind)> = self
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(i, t)| (t.stream == stream_id).then_some((i, t.kind)))
            .collect();

        // For each affected stream, if it's got a viewport, update its filter.
        // Other affected tabs go through the synchronous refresh
        // path (Stream tabs) or the summary-build queue (Summary
        // tabs) — they aren't user-visible during the active tab's
        // long op, so the freeze that a selective filter causes is
        // hidden behind the active tab's progress bar.
        // XXX-dap get rid of the queued summary builds
        for (i, kind) in affected {
            match kind {
                TabKind::Stream => {
                    if let Some(v) = &mut self.tabs[i].viewport {
                        v.set_filter(&self.engine, new_filter.clone());
                    }
                }
                TabKind::Summary => {
                    self.enqueue_summary_build(TabIdx(i), new_filter.clone());
                }
            };
        }

        self.save_after_inline_mutation();
    }

    /// Toggles whether the active stream renders structured fields
    /// beyond the bunyan header.  Persisted on the [`LogStream`] so the
    /// preference outlives the session, and applied to every tab
    /// targeting that stream so two tabs sharing a stream stay
    /// consistent.  Preserves each tab's anchor record (only the
    /// rendering changed; the events themselves did not).
    fn toggle_show_extras(&mut self) {
        let stream_id = self.tabs[self.active].stream;
        let Some(mut stream) = self.session.streams.remove(&stream_id) else {
            return;
        };
        stream.show_extras = !stream.show_extras;
        self.rerender_after_stream_mutation(stream_id, stream);
        self.save_after_inline_mutation();
    }

    /// Toggles whether the leading timestamp on each rendered line
    /// includes its `YYYY-MM-DD` prefix.  Persisted on the [`LogStream`]
    /// so the preference outlives the session, and applied to every tab
    /// targeting that stream so two tabs sharing a stream stay
    /// consistent.
    fn toggle_show_date(&mut self) {
        let stream_id = self.tabs[self.active].stream;
        let Some(mut stream) = self.session.streams.remove(&stream_id) else {
            return;
        };
        stream.show_date = !stream.show_date;
        self.rerender_after_stream_mutation(stream_id, stream);
        self.save_after_inline_mutation();
    }

    /// Toggles raw rendering on the active stream.  In raw mode each
    /// record renders as its bytes from the source rather than the
    /// formatted header/extras layout; the other column toggles
    /// (date, hostname, name/pid, extras) are ignored.  Persisted on
    /// the [`LogStream`] and applied to every tab targeting it.
    fn toggle_show_raw(&mut self) {
        let stream_id = self.tabs[self.active].stream;
        let Some(mut stream) = self.session.streams.remove(&stream_id) else {
            return;
        };
        stream.show_raw = !stream.show_raw;
        self.rerender_after_stream_mutation(stream_id, stream);
        self.save_after_inline_mutation();
    }

    /// Replaces the active stream's [`RenderOpts`] with `opts`,
    /// persists it, and re-renders every tab targeting that stream.
    /// Used by the `d` field-display dialog, which mutates several
    /// knobs at once.
    fn apply_render_opts(&mut self, opts: RenderOpts) {
        let stream_id = self.tabs[self.active].stream;
        let Some(mut stream) = self.session.streams.remove(&stream_id) else {
            return;
        };
        stream.set_render_opts(opts);
        self.rerender_after_stream_mutation(stream_id, stream);
        self.save_after_inline_mutation();
    }

    /// Re-inserts `stream` into the session and triggers a `rerender`
    /// on every tab targeting it.  Shared by the `F`/`D` toggles and
    /// the `d` field-display dialog: each mutates one or more
    /// rendering knobs and wants every tab to repaint with the new
    /// values.
    fn rerender_after_stream_mutation(
        &mut self,
        stream_id: LogStreamId,
        stream: LogStream,
    ) {
        let new_filter = stream.filter.clone();
        let opts = stream.render_opts();
        self.session
            .streams
            .insert_unique(stream)
            .expect("removed-then-reinserted id is unique");
        for tab in self.tabs.iter_mut() {
            if tab.stream == stream_id {
                tab.rerender(&self.engine, &new_filter, opts);
            }
        }
    }

    /// Returns the [`LogStream`] backing the active tab.  All read-only
    /// access to the active stream's filter or render fields goes
    /// through here, so the single `.expect("stream exists")` lives in
    /// one place; the invariant is that every tab's `stream` id is
    /// owned by `session.streams`.
    fn active_stream(&self) -> &LogStream {
        let stream_id = self.tabs[self.active].stream;
        self.session.streams.get(&stream_id).expect("stream exists")
    }

    /// Replaces the active tab's display name.  The new name is
    /// persisted on the next save so a resumed session shows it on the
    /// restored tab strip; the rename is treated as a low-cadence
    /// mutation (the user just typed Enter) and flushes inline.
    fn rename_active_tab(&mut self, name: String) {
        self.tabs[self.active].name = name;
        self.save_after_inline_mutation();
    }

    /// Opens the read-only popup showing the `seeit` command that
    /// would reproduce the active view (the `--tab` form).
    ///
    /// Wrapper around [`Self::open_seeit_command_dialog_with`] that
    /// picks the active tab's name as the selector — the right form
    /// for the regular-tab `Y` binding because the tab's saved cursor
    /// and TabKind come along for the ride, so Summary tabs reproduce
    /// as summaries.
    fn open_seeit_command_dialog(&mut self) {
        let selector = Selector::Tab(self.active_tab().name.clone());
        self.open_seeit_command_dialog_with(selector);
    }

    /// Opens the read-only `seeit`-command popup for an arbitrary
    /// [`Selector`].
    ///
    /// Saves the session first so the on-disk state matches what the
    /// user is looking at — the printed command's claims about
    /// filter, cursor, and field visibility would otherwise lag the
    /// user's most recent edits.  When the App has no
    /// [`SessionStore`] attached (a transient session passed via
    /// `--no-resume` or similar), the command isn't meaningful
    /// because there's nothing on disk to point `seeit` at; the
    /// keybinding falls back to a notice.
    fn open_seeit_command_dialog_with(&mut self, selector: Selector) {
        if self.store.is_none() {
            self.notice = Some(
                "seeit reproduction needs a saved session (this one \
                 is transient)"
                    .to_string(),
            );
            return;
        }
        if let Err(e) = self.try_save_now() {
            self.notice = Some(format!("save before seeit failed: {e}"));
            return;
        }
        let cmd = build_seeit_command(self.session.id, &selector);
        self.dialog = Some(Dialog::seeit_command(cmd));
    }

    /// Opens the `seeit`-command popup for the bookmark under the
    /// Bookmarks-tab cursor.  Uses [`BookmarkId`]'s full UUID display
    /// form as the selector needle — guaranteed unambiguous regardless
    /// of whether the bookmark is named or whether two bookmarks share
    /// a name.  No-op if the cursor isn't pointing at an existing
    /// bookmark (defensive: the caller arms this only when
    /// `flat_bookmarks` is non-empty and initializes the cursor first).
    fn open_seeit_command_for_bookmark_cursor(&mut self) {
        let Some(idx) = self.bookmark_cursor_idx() else {
            return;
        };
        let bm_id = self.flat_bookmarks()[idx].id;
        let selector = Selector::Bookmark(bm_id.to_string());
        self.open_seeit_command_dialog_with(selector);
    }

    /// Adds a bookmark to the session, filed under the active tab's
    /// stream.  If this is the user's first bookmark the synthetic
    /// Bookmarks tab will appear in the next render; we leave the
    /// active pane on the current tab — opening Bookmarks
    /// automatically would surprise users who just made one bookmark
    /// and want to keep reading.
    fn add_bookmark(
        &mut self,
        name: Option<BookmarkName>,
        draft: BookmarkDraft,
    ) {
        let stream_id = self.tabs[self.active].stream;
        let bookmark = Bookmark {
            id: BookmarkId::new_v4(),
            created_at: chrono::Utc::now(),
            cursor: draft.cursor,
            name,
            display_source: draft.display_source,
            display_time: draft.display_time,
            display_name: draft.display_name,
            display_msg: draft.display_msg,
        };
        self.session.add_bookmark(stream_id, bookmark);
        self.save_after_inline_mutation();
    }

    /// Removes a bookmark by id.  When this empties the user's
    /// bookmarks the Bookmarks tab disappears next render; if it was
    /// the active pane, fall back to the last regular tab.
    fn delete_bookmark(&mut self, id: BookmarkId) {
        let was_bookmarks_active = self.bookmarks_active();
        self.session.remove_bookmark(id);
        if Some(id) == self.bookmark_cursor {
            self.bookmark_cursor = None;
        }
        // If we removed the last bookmark and the user was looking at
        // the Bookmarks tab, snap them back to the last regular tab so
        // they don't end up looking at a vanished pane.
        if was_bookmarks_active && !self.has_bookmarks_tab() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        self.save_after_inline_mutation();
    }

    /// All user bookmarks, flattened across streams and sorted by the
    /// timestamp of the bookmarked event (`display_time`), with
    /// `created_at` breaking ties so the order is total and stable.
    /// This is the same order the Bookmarks tab renders rows in, and
    /// the order `j`/`k` walk; sorting by event time lets the user
    /// read the list chronologically regardless of when each bookmark
    /// was created.
    fn flat_bookmarks(&self) -> Vec<&Bookmark> {
        let mut bms: Vec<&Bookmark> = self
            .session
            .user_bookmarks
            .values()
            .flat_map(|v| v.iter())
            .collect();
        bms.sort_by(|a, b| {
            a.display_time
                .cmp(&b.display_time)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
        bms
    }

    /// Index of `bookmark_cursor` in [`Self::flat_bookmarks`], if it
    /// still exists.  Returns `None` if the cursor refers to a
    /// since-deleted bookmark or no cursor is set.
    fn bookmark_cursor_idx(&self) -> Option<usize> {
        let target = self.bookmark_cursor?;
        self.flat_bookmarks().iter().position(|b| b.id == target)
    }

    /// Moves the bookmark-pane cursor by `delta` rows (positive ==
    /// down).  When no cursor is set, this initializes it at the first
    /// bookmark.  No-op when the bookmark list is empty.
    fn move_bookmark_cursor(&mut self, delta: isize) {
        let bookmarks = self.flat_bookmarks();
        if bookmarks.is_empty() {
            self.bookmark_cursor = None;
            return;
        }
        let last = bookmarks.len() - 1;
        let cur = self.bookmark_cursor_idx().unwrap_or_default();
        let new = (cur as isize + delta).clamp(0, last as isize) as usize;
        self.bookmark_cursor = Some(bookmarks[new].id);
    }

    /// Navigates to the bookmark at the bookmark-pane cursor.  Switches
    /// to the existing tab if its stream is open, otherwise opens a
    /// new tab targeting that stream.  Seeks the tab's streamview to
    /// the bookmark's saved cursor; on a filter mismatch stashes a
    /// one-shot footer notice so the user knows the anchor landed on
    /// the nearest visible entry instead of the bookmarked one.
    fn navigate_to_bookmark_cursor(&mut self) {
        let Some(idx) = self.bookmark_cursor_idx() else {
            return;
        };
        self.policy.record(Cadence::Debounced);
        let bookmarks = self.flat_bookmarks();
        let bm = bookmarks[idx];
        let (target_stream, filter) = self
            .session
            .user_bookmarks
            .iter()
            .find_map(|(sid, v)| {
                v.iter()
                    .any(|b| b.id == bm.id)
                    .then(|| {
                        self.session
                            .streams
                            .get(sid)
                            .map(|s| (*sid, s.filter.clone()))
                    })
                    .flatten()
            })
            .expect("bookmark belongs to a known stream");
        let cursor = bm.cursor.clone();
        // Switch to the tab showing the target stream, opening one if
        // none exists.
        let existing = self.tabs.iter().position(|t| t.stream == target_stream);
        let tab_idx = match existing {
            Some(i) => i,
            None => {
                self.push_tab_for_existing_stream(target_stream);
                self.tabs.len() - 1
            }
        };
        self.active = tab_idx;
        // Decide whether the bookmarked event survives the active
        // filter by reading it through an unfiltered stepper.  The
        // streamview's seek will hit it (or the nearest visible
        // neighbor) regardless; the check just feeds the post-jump
        // footer notice.
        let bookmarked_passes_filter = self
            .engine
            .stepper(Filter::default(), &cursor)
            .step_forward()
            .is_some_and(|r| {
                r.event().as_ref().is_ok_and(|e| filter.matches_event(e))
            });
        // `tab_idx` was just made active above, so this routes through
        // the standard seek-active-to-cursor path that installs a
        // [`LongOp::Seek`] (progress bar + Ctrl-C cancellation).
        // Test fixtures without a streamview no-op there, which
        // matches the prior synchronous behavior.
        self.seek_active_to_cursor(cursor);
        if !bookmarked_passes_filter {
            self.notice = Some(
                "bookmarked entry is hidden by the active filter; \
                 jumped to the nearest visible entry"
                    .to_string(),
            );
        }
    }

    /// Installs `regex` as the active search on the current tab,
    /// records it as the most recent search at the app level,
    /// promotes it to the front of [`Session::search_history`], and
    /// scrolls so the first match at or after the current viewport
    /// top sits at the top of the viewport.  "At or after" applies
    /// to fresh searches only — repeats and `n`/`N` advance strictly
    /// past the current top via [`Self::step_search`].
    fn apply_search(
        &mut self,
        pattern: String,
        regex: Regex,
        direction: SearchDirection,
    ) {
        let active = self.active;
        let matches = self.tabs[active].match_indices(&regex);
        self.tabs[active].search =
            Some(TabSearch { pattern: pattern.clone(), regex, matches });
        self.session.record_search(&pattern);
        self.last_search = Some(LastSearch { pattern, direction });
        self.jump_to_match(direction, SearchAnchor::Include);
    }

    /// Repeats the most recent search (used by `/<enter>` and
    /// `?<enter>` with an empty buffer).  Updates the stored direction
    /// so a follow-up `n` continues the way the user just chose, and
    /// bumps the pattern to the front of
    /// [`Session::search_history`] so re-running a search through the
    /// prompt counts as a use.  No-op if there is no previous search.
    fn repeat_last_search(&mut self, direction: SearchDirection) {
        let pattern = match &self.last_search {
            Some(l) => l.pattern.clone(),
            None => return,
        };
        self.ensure_tab_search(&pattern);
        self.session.record_search(&pattern);
        self.last_search = Some(LastSearch { pattern, direction });
        self.jump_to_match(direction, SearchAnchor::Skip);
    }

    /// Steps to the next match in `direction` without changing which
    /// direction was the user's last expressed preference.  Used by
    /// `n` (direction = stored direction) and `N` (direction =
    /// opposite).  No-op if there is no previous search.
    fn step_search(&mut self, direction: SearchDirection) {
        let pattern = match &self.last_search {
            Some(l) => l.pattern.clone(),
            None => return,
        };
        self.ensure_tab_search(&pattern);
        self.jump_to_match(direction, SearchAnchor::Skip);
    }

    /// Re-derives `tab.search` from `pattern` if it's missing or stale.
    /// Called before any repeat to recover from filter edits (which
    /// clear `tab.search`) and from tab switches.
    fn ensure_tab_search(&mut self, pattern: &str) {
        let tab = &mut self.tabs[self.active];
        let needs_recompute = match &tab.search {
            Some(s) => s.pattern != pattern,
            None => true,
        };
        if !needs_recompute {
            return;
        }
        // The regex came from `last_search`, which was only set after a
        // successful compile, so this should not fail.  If it somehow
        // does (e.g. a future code path puts an invalid pattern there),
        // leave `tab.search` empty rather than panicking.
        let Ok(regex) = Regex::new(pattern) else {
            return;
        };
        let matches = tab.match_indices(&regex);
        tab.search =
            Some(TabSearch { pattern: pattern.to_string(), regex, matches });
    }

    /// Moves the viewport to the next match in `direction`.
    ///
    /// For tabs with a [`StreamView`]: walks the engine lazily via
    /// [`StreamView::search_step`], extending the window as needed.
    /// For tabs without one (test fixtures, Summary): scans the
    /// precomputed `matches` list relative to `viewport_top`.
    ///
    /// `anchor`: pass [`SearchAnchor::Skip`] to skip a match at the
    /// current position (used by `n` repeats so the cursor advances
    /// rather than re-landing) and [`SearchAnchor::Include`] otherwise.
    fn jump_to_match(
        &mut self,
        direction: SearchDirection,
        anchor: SearchAnchor,
    ) {
        self.policy.record(Cadence::Debounced);
        let active = self.active;
        if self.tabs[active].viewport.is_some() {
            self.jump_to_match_via_streamview(direction, anchor);
        } else {
            self.jump_to_match_via_matches(direction, anchor);
        }
    }

    fn jump_to_match_via_streamview(
        &mut self,
        direction: SearchDirection,
        _anchor: SearchAnchor, // XXX-dap
    ) {
        let active = self.active;
        let Some(regex) =
            self.tabs[active].search.as_ref().map(|s| s.regex.clone())
        else {
            return;
        };
        // XXX-dap why do we have Direction, SearchDirection, and SearchDir
        let dir = match direction {
            SearchDirection::Forward => Direction::Forward,
            SearchDirection::Backward => Direction::Backward,
        };
        // Snapshot the streamview's running parse stats so the search
        // op can diff against them and report bytes/records consumed
        // *just for this search* (as opposed to the streamview's
        // lifetime totals, which include every scroll and back-fetch
        // since the filter was last set).
        let view = self.tabs[active]
            .viewport
            .as_mut()
            .expect("caller checked streamview is_some");
        view.start_seek_for_search(dir, regex);
    }

    fn jump_to_match_via_matches(
        &mut self,
        direction: SearchDirection,
        anchor: SearchAnchor,
    ) {
        let tab = &self.tabs[self.active];
        let Some(search) = &tab.search else {
            return;
        };
        let cur = tab.viewport_top.get();
        let target = match (direction, anchor) {
            (SearchDirection::Forward, SearchAnchor::Skip) => {
                search.matches.iter().copied().find(|&m| m > cur)
            }
            (SearchDirection::Forward, SearchAnchor::Include) => {
                search.matches.iter().copied().find(|&m| m >= cur)
            }
            (SearchDirection::Backward, SearchAnchor::Skip) => {
                search.matches.iter().rev().copied().find(|&m| m < cur)
            }
            (SearchDirection::Backward, SearchAnchor::Include) => {
                search.matches.iter().rev().copied().find(|&m| m <= cur)
            }
        };
        if let Some(t) = target {
            self.tabs[self.active].viewport_top = LineIdx(t);
        }
    }

    /// Display label for the current time-navigation step (e.g. `"1m"`).
    fn current_step_label(&self) -> &'static str {
        TIME_STEPS[self.time_step_idx].0
    }

    fn current_step_duration(&self) -> chrono::Duration {
        chrono::Duration::milliseconds(TIME_STEPS[self.time_step_idx].1)
    }

    /// Bumps the step to the next-larger value, clamped at the largest.
    /// Clamping (rather than wrapping) means a user mashing `=` can't
    /// accidentally jump from "1d" back to "100ms".
    fn increase_time_step(&mut self) {
        if self.time_step_idx + 1 < TIME_STEPS.len() {
            self.time_step_idx += 1;
        }
    }

    /// Bumps the step to the next-smaller value, clamped at the smallest.
    fn decrease_time_step(&mut self) {
        if self.time_step_idx > 0 {
            self.time_step_idx -= 1;
        }
    }

    /// Advances the active tab forward (or backward) by the current
    /// step.  No-op when the tab holds no parsed events.
    fn advance_time(&mut self, dir: Direction) {
        self.policy.record(Cadence::Debounced);
        let delta = self.current_step_duration();
        let h = self.viewport_height;
        let w = self.viewport_width;
        let active = self.active;
        self.tabs[active].advance_time(dir, delta, h, w);
    }

    fn next_tab(&mut self) {
        let n = self.pane_count();
        self.active = (self.active + 1) % n;
        // Switching panes resets a one-shot notice so it doesn't spook
        // the user on an unrelated screen.
        self.notice = None;
    }

    fn prev_tab(&mut self) {
        let n = self.pane_count();
        self.active = (self.active + n - 1) % n;
        self.notice = None;
    }

    /// Removes the active tab.  When the last regular tab is closed a
    /// fresh default tab is created so the `!tabs.is_empty()`
    /// invariant holds.  No-op when the synthetic Bookmarks tab is
    /// active: that pane is implicit and cannot be explicitly closed.
    fn close_active_tab(&mut self) {
        if self.bookmarks_active() {
            return;
        }
        let closed = self.active;
        // Detach any long-op state that referred to the closed tab,
        // and renumber state that referred to a later tab now that
        // its index has dropped by one.
        // XXX-dap
        // self.adjust_long_op_state_for_closed_tab(closed);
        self.tabs.remove(closed);
        if self.tabs.is_empty() {
            // push_tab will itself save inline.  The outer save below
            // is therefore redundant in this branch — accepted for
            // simplicity, since saves are atomic and cheap.
            self.push_tab(TabKind::Stream, Filter::default());
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        self.save_after_inline_mutation();
    }

    // XXX-dap
    //     /// Patches up [`Self::long_op`] and [`Self::pending_summary_builds`]
    //     /// for a tab being removed at `closed`.  Long ops bound to the
    //     /// closed tab are cancelled (no destination to write back to);
    //     /// long ops bound to a later tab have their tab index decremented
    //     /// to follow the shift in `self.tabs`.  Same renumbering applies
    //     /// to queued Summary builds.
    //     fn adjust_long_op_state_for_closed_tab(&mut self, closed: usize) {
    //         let closed = TabIdx(closed);
    //         let shift_down = |idx: &mut TabIdx| {
    //             *idx = idx.prev();
    //         };
    //         match self.long_op.as_mut() {
    //             Some(LongOp::BuildSummary(s)) => {
    //                 if s.tab_idx == closed {
    //                     self.long_op = None;
    //                 } else if s.tab_idx > closed {
    //                     shift_down(&mut s.tab_idx);
    //                 }
    //             }
    //             Some(LongOp::Search(s)) => {
    //                 if s.tab_idx == closed {
    //                     self.long_op = None;
    //                 } else if s.tab_idx > closed {
    //                     shift_down(&mut s.tab_idx);
    //                 }
    //             }
    //             Some(LongOp::Seek(s)) => {
    //                 if s.tab_idx == closed {
    //                     self.long_op = None;
    //                 } else if s.tab_idx > closed {
    //                     shift_down(&mut s.tab_idx);
    //                 }
    //             }
    //             None => {}
    //         }
    //         self.pending_summary_builds.retain(|(idx, _)| *idx != closed);
    //         for (idx, _) in self.pending_summary_builds.iter_mut() {
    //             if *idx > closed {
    //                 shift_down(idx);
    //             }
    //         }
    //     }

    /// Enters select mode on the active tab with the given action.
    /// The selection starts on the record at the top of the viewport
    /// (or the record whose extras the user is currently scrolled
    /// into).  No-op when the tab has no records.
    fn start_selection(&mut self, action: SelectionAction) {
        let tab = self.active_tab_mut();
        if tab.events().is_empty() {
            return;
        }
        // event_for_line is empty iff formatted is empty iff events is
        // empty (handled above).  Anything past the last line is
        // similarly impossible after the empty check, but be defensive
        // against a viewport_top set out of range by a future caller.
        let event_idx = tab
            .event_for_line()
            .get(tab.viewport_top.get())
            .copied()
            .unwrap_or(EventIdx(tab.events().len() - 1));
        tab.select = Some(Selection { event_idx, action });
    }

    /// Routes a keystroke while the Bookmarks pane is active.
    ///
    /// Supported keys: j/k (move bookmark cursor), Enter (navigate to
    /// the bookmark — switches tabs or opens a new one), x (open the
    /// delete-confirmation dialog), Y (open the `seeit`-command popup
    /// for the bookmark under the cursor), Tab/BackTab (cycle panes),
    /// q (open the quit-confirmation dialog), h (open help).
    /// Everything else is dropped: filter edits, search, time-step
    /// navigation, and Ctrl-T/Ctrl-W make no sense in a list of
    /// bookmarks and would leave the user in a confusing state if
    /// half-handled.
    fn handle_bookmarks_key(&mut self, key: KeyEvent) {
        match key {
            // Only `q` opens the quit prompt: Esc and Ctrl-C are easy
            // to hit by accident (Esc on a misfired dialog cancel,
            // Ctrl-C from terminal muscle memory) and an unwanted quit
            // would tear down the user's in-flight viewport, search,
            // and any unsaved exclude/include drafts.
            KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.dialog = Some(Dialog::confirm_quit());
            }
            KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.next_tab(),
            KeyEvent { code: KeyCode::BackTab, .. }
            | KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::SHIFT,
                ..
            } => self.prev_tab(),
            KeyEvent {
                code: KeyCode::Char('j') | KeyCode::Down,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_bookmark_cursor(1),
            KeyEvent {
                code: KeyCode::Char('k') | KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_bookmark_cursor(-1),
            // `Y`: open the seeit-reproduction popup for the bookmark
            // under the cursor — the Bookmarks pane's cursor *is* the
            // selection, so there's no separate "arm" step.  Mirrors
            // the regular-tab binding's modifier handling: terminals
            // report `Y` with either NONE or SHIFT, accept both.  When
            // no row is highlighted yet, snap the cursor to the first
            // bookmark so the keypress isn't a silent no-op.
            KeyEvent { code: KeyCode::Char('Y'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                if self.bookmark_cursor_idx().is_none() {
                    self.move_bookmark_cursor(0);
                }
                self.open_seeit_command_for_bookmark_cursor();
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.navigate_to_bookmark_cursor(),
            KeyEvent {
                code: KeyCode::Char('x'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                if let Some(idx) = self.bookmark_cursor_idx() {
                    let bm = self.flat_bookmarks()[idx];
                    let label = match &bm.name {
                        Some(n) => format!("\"{}\"", n),
                        None => preview_msg(&bm.display_msg),
                    };
                    self.dialog =
                        Some(Dialog::confirm_delete_bookmark(bm.id, label));
                }
            }
            // `h`: open the help popup.  Matches `less` and keeps the
            // binding consistent with the main-tab keymap.
            KeyEvent {
                code: KeyCode::Char('h'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.dialog = Some(Dialog::help());
            }
            _ => {}
        }
    }

    /// Routes a keystroke while select mode is active on the current
    /// tab.  Only the keys that make sense in this transient mode are
    /// honored; everything else is dropped on the floor so a stray `f`
    /// or `^T` can't move the user into a context where the row index
    /// no longer means what they thought.
    fn handle_selection_key(&mut self, key: KeyEvent) {
        let h = self.viewport_height;
        match key {
            KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.active_tab_mut().select = None;
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.commit_selection();
            }
            KeyEvent {
                code: KeyCode::Char('j') | KeyCode::Down,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.active_tab_mut().move_selection(1, h);
            }
            KeyEvent {
                code: KeyCode::Char('k') | KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.active_tab_mut().move_selection(-1, h);
            }
            _ => {}
        }
    }

    /// Commits the in-progress selection.
    ///
    /// For `Exclude` / `Include`: build a `msg=<selected msg>` (or
    /// `msg!=<selected msg>`) predicate from the row under the
    /// highlight, append it to the host stream's filter, and re-query.
    /// For `Bookmark`: open the bookmark-name dialog with the row's
    /// position pinned (committed when the dialog applies).
    ///
    /// When the selected row is an error (no parsed event) this is a
    /// no-op: there's no `msg` to extract or position to anchor, and
    /// silently exiting the mode would be more confusing than just
    /// doing nothing and letting the user pick a different row or Esc
    /// out.
    fn commit_selection(&mut self) {
        let tab = &self.tabs[self.active];
        let Some(sel) = tab.select else {
            return;
        };
        let Some(Row::Event(ee)) = tab.events().get(sel.event_idx.get()) else {
            return;
        };
        match sel.action {
            SelectionAction::Exclude | SelectionAction::Include => {
                // Outer arm guarantees `sel.action` is one of these two;
                // `Exclude` → `Form::Negated`, `Include` → `Form::Affirmed`.
                let form = if matches!(sel.action, SelectionAction::Exclude) {
                    Form::Negated
                } else {
                    Form::Affirmed
                };
                let new_pred = EventPredicate::FieldEquals {
                    name: FieldName::Core(CoreField::Msg),
                    value: ee.event.msg.clone(),
                    form,
                }
                .into();
                let mut new_filter = self.active_stream().filter.clone();
                new_filter.add_predicate(new_pred);
                // apply_filter clears search and select; the viewport
                // slides to the nearest visible record under the new
                // filter rather than snapping to the top, so the user
                // stays where they were.
                self.apply_filter(new_filter);
            }
            SelectionAction::Bookmark => {
                // Production tabs derive the cursor directly from the
                // streamview's window — the selected record is at
                // `event_idx` in `view.records()`, and the window
                // already knows its `front_cursor` plus every record
                // preceding the selection.  Test fixtures without a
                // streamview synthesize events and have no source on
                // the engine, so fall back to a default cursor; the
                // bookmark still renders and can be deleted.
                let cursor = tab
                    .viewport
                    .as_ref()
                    .and_then(|v| v.cursor_before_record(sel.event_idx))
                    .unwrap_or_default();
                let draft = BookmarkDraft {
                    cursor,
                    display_source: ee.position.source().clone(),
                    display_time: ee.event.time,
                    display_name: ee.event.name.to_string(),
                    display_msg: preview_msg(&ee.event.msg),
                };
                self.dialog = Some(Dialog::bookmark_name(draft));
                self.active_tab_mut().select = None;
            }
        }
    }

    /// Test constructor that skips the engine entirely.  The scroll
    /// and search tests don't care where rows come from, and
    /// unit-testing those against pre-formatted strings is much simpler
    /// than wiring up real log files.  The events vec is parallel-empty
    /// (`None` everywhere); tests that exercise exclude mode go through
    /// [`App::with_events`] instead.
    #[cfg(test)]
    fn with_rows(formatted: Vec<String>) -> Self {
        let events = vec![None; formatted.len()];
        Self::with_events(events, formatted)
    }

    /// Test constructor that lets the caller supply both events and
    /// pre-formatted display strings.  Required for exclude-mode tests,
    /// which read `Event::msg` to build the new predicate.
    ///
    /// Each `formatted[i]` is treated as the single display line for
    /// `events[i]` — the multi-line render path runs only through
    /// [`render_rows`], so synthetic-input tests stay 1:1 between
    /// records and lines.
    #[cfg(test)]
    fn with_events(
        events: Vec<Option<LogEvent>>,
        formatted: Vec<String>,
    ) -> Self {
        assert_eq!(events.len(), formatted.len());
        let mut a = Self {
            engine: Engine::new(),
            session: Session::new(),
            tabs: Vec::new(),
            active: 0,
            // The first push_tab below consumes "Tab 1".
            next_tab_number: 1,
            viewport_height: 0,
            viewport_width: 0,
            quit: false,
            dialog: None,
            last_search: None,
            time_step_idx: DEFAULT_TIME_STEP_IDX,
            bookmark_cursor: None,
            notice: None,
            pending_summary_builds: std::collections::VecDeque::new(),
            show_fetch_stats: false,
            store: None,
            policy: SavePolicy::new(SavePolicy::DEFAULT_DEBOUNCE),
        };
        // Manually push so we can override the row data (the engine has
        // no sources, so a real push_tab would yield empty vecs).
        // Wrap each pre-built `LogEvent` in an `EngineEvent` so the
        // bookmark/exclude paths can reach `.event` and `.position`
        // uniformly.  The synthetic position uses a fixed source id and
        // the event's own time; tests that don't bookmark don't care.
        let stream = LogStream::new(format!("Tab {}", a.next_tab_number));
        let stream_id = stream.id;
        a.session.streams.insert_unique(stream).expect("unique id");
        let synthetic_source = SourceId::from("test".to_string());
        // Test rows: `Some(event)` becomes a real [`Row::Event`]; `None`
        // becomes a [`Row::Error`] whose error text is whatever the
        // caller put in the matching `formatted` slot, since tests use
        // the formatted line as the stand-in display message.
        let engine_events: Vec<Row> = events
            .into_iter()
            .zip(&formatted)
            .map(|(maybe, line)| match maybe {
                Some(event) => Row::Event(EngineEvent {
                    position: LogStreamPosition::new(
                        synthetic_source.clone(),
                        event.time,
                        0,
                    ),
                    event,
                }),
                None => Row::Error(line.clone()),
            })
            .collect();
        let event_for_line: Vec<EventIdx> =
            (0..formatted.len()).map(EventIdx).collect();
        let first_line_for_event: Vec<LineIdx> =
            (0..engine_events.len()).map(LineIdx).collect();
        a.tabs.push(Tab {
            name: format!("Tab {}", a.next_tab_number),
            stream: stream_id,
            kind: TabKind::Stream,
            // Test fixtures bypass the engine; no streamview to feed
            // off.  `Tab::maintain_window` and the seek helpers are
            // no-ops in this case so the materialized vectors below
            // stay authoritative.
            streamview: None,
            standalone_materialized: Materialized {
                events: engine_events,
                formatted,
                event_for_line,
                first_line_for_event,
                parse_stats: ParseStats::default(),
            },
            viewport_top: LineIdx::ZERO,
            search: None,
            select: None,
        });
        a.next_tab_number += 1;
        a
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Windows reports Press, Repeat, and Release; ignore the latter
        // two so a single keystroke isn't doubled.
        if key.kind != KeyEventKind::Press {
            return;
        }
        // While any dialog is open it gets every keystroke — otherwise
        // typing `q` or `j` into the editor would quit or scroll the
        // underlying view, and Ctrl-W would close the host tab from
        // under the dialog.
        if let Some(dialog) = self.dialog.as_mut() {
            match dialog.handle_key(key) {
                DialogResult::Stay => {}
                DialogResult::Cancel => self.dialog = None,
                DialogResult::ApplyFilter(filter) => {
                    self.dialog = None;
                    self.apply_filter(filter);
                }
                DialogResult::ApplyRename(name) => {
                    self.dialog = None;
                    self.rename_active_tab(name);
                }
                DialogResult::ApplySearch { pattern, regex, direction } => {
                    self.dialog = None;
                    self.apply_search(pattern, regex, direction);
                }
                DialogResult::RepeatSearch(direction) => {
                    self.dialog = None;
                    self.repeat_last_search(direction);
                }
                DialogResult::ApplyBookmark { name, draft } => {
                    self.dialog = None;
                    self.add_bookmark(name, draft);
                }
                DialogResult::ApplyDeleteBookmark(id) => {
                    self.dialog = None;
                    self.delete_bookmark(id);
                }
                DialogResult::ApplyQuit => {
                    self.dialog = None;
                    self.quit = true;
                }
                DialogResult::ApplyDisplayFields(opts) => {
                    self.dialog = None;
                    self.apply_render_opts(opts);
                }
            }
            return;
        }
        // Bookmarks pane has its own narrow keymap (j/k/Enter/x +
        // Tab cycling).  Routing it here, ahead of the regular-tab
        // dispatch, keeps the cursor model decoupled from the
        // log-stream tabs' viewport/selection state.
        if self.bookmarks_active() {
            self.handle_bookmarks_key(key);
            return;
        }
        // Exclude mode similarly intercepts every keystroke: in this
        // mode j/k move the selection rather than the viewport, and
        // Enter commits the new exclusion predicate.  Other actions
        // (filter edits, tab switches, search) are intentionally
        // suppressed so the in-progress selection can't be carried into
        // a context where the row index would no longer mean what the
        // user thought.
        if self.active_tab().select.is_some() {
            self.handle_selection_key(key);
            return;
        }
        // Any other action clears a pending one-shot notice.
        self.notice = None;
        let page = self.viewport_height as usize;
        let half_page = page / 2;
        match key {
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.interrupt();
            }

            KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.dialog = Some(Dialog::confirm_quit());
            }
            KeyEvent {
                code: KeyCode::Char('j') | KeyCode::Down,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let h = self.viewport_height;
                let w = self.viewport_width;
                let active = self.active;
                self.tabs[active].scroll_down(1, h, w);
                self.policy.record(Cadence::Debounced);
            }
            KeyEvent {
                code: KeyCode::Char('k') | KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let h = self.viewport_height;
                let w = self.viewport_width;
                let active = self.active;
                self.tabs[active].scroll_up(1, h, w);
                self.policy.record(Cadence::Debounced);
            }
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                let h = self.viewport_height;
                let w = self.viewport_width;
                let active = self.active;
                self.tabs[active].scroll_down(half_page, h, w);
                self.policy.record(Cadence::Debounced);
            }
            KeyEvent {
                code: KeyCode::Char(' '),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let h = self.viewport_height;
                let w = self.viewport_width;
                let active = self.active;
                self.tabs[active].scroll_down(page, h, w);
                self.policy.record(Cadence::Debounced);
            }
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                let h = self.viewport_height;
                let w = self.viewport_width;
                let active = self.active;
                self.tabs[active].scroll_up(half_page, h, w);
                self.policy.record(Cadence::Debounced);
            }
            KeyEvent {
                code: KeyCode::Char('g') | KeyCode::Home,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.seek_active_to_start();
            }
            // Different terminals report `G` with NONE or SHIFT; accept
            // both.
            KeyEvent { code: KeyCode::Char('G'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.seek_active_to_end();
            }
            KeyEvent {
                code: KeyCode::End,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.seek_active_to_end();
            }
            KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.dialog =
                    Some(Dialog::filter(&self.active_stream().filter));
            }
            // `F`: toggle whether the active stream's structured-field
            // extras render below the bunyan header.  Some terminals
            // report `F` with NONE, others with SHIFT (matching `G` /
            // `?` / `X`); accept both.
            KeyEvent { code: KeyCode::Char('F'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.toggle_show_extras();
            }
            // `D`: toggle whether the leading timestamp carries its
            // date prefix.  Same NONE/SHIFT permissiveness as `F`.
            KeyEvent { code: KeyCode::Char('D'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.toggle_show_date();
            }
            // `R`: toggle raw rendering — show each record's bytes
            // from the source instead of the formatted header/extras
            // layout.  Useful for inspecting fields the parser dropped
            // or normalized.  NONE/SHIFT both accepted, like `D`/`F`.
            KeyEvent { code: KeyCode::Char('R'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.toggle_show_raw();
            }
            // `p`: toggle the developer-oriented engine-fetch-progress
            // row.  Off by default — it tracks engine work, not what a
            // typical user wants to read, and the numbers are confusing
            // out of context.
            KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.show_fetch_stats = !self.show_fetch_stats;
            }
            // `d`: open the field-display dialog (timestamp format,
            // hostname mode, name/pid/extras visibility).  Folds the
            // `F` extras toggle alongside the other knobs in one place
            // — `F` keeps its shortcut for muscle memory.  `h` is
            // reserved for the help popup (matching `less`), so the
            // display-fields dialog lives on `d`.
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.dialog = Some(Dialog::display_fields(
                    self.active_stream().render_opts(),
                ));
            }
            // `h`: open the keybindings help popup (matching `less`).
            KeyEvent {
                code: KeyCode::Char('h'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.dialog = Some(Dialog::help());
            }
            // `x`: enter select mode for *exclusion*; `X`: same mode
            // but for *inclusion*; `b`: same mode but for bookmarking
            // the row under the highlight.  Different terminals report
            // `X` with either NONE or SHIFT modifiers (matching `G`/
            // `?`), so we accept both.  The selection starts at
            // `viewport_top` so the user can immediately move it
            // through the visible rows; if the tab is empty there's
            // nothing to select against, so this is a no-op.
            KeyEvent {
                code: KeyCode::Char('x'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.start_selection(SelectionAction::Exclude);
            }
            KeyEvent { code: KeyCode::Char('X'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.start_selection(SelectionAction::Include);
            }
            KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.start_selection(SelectionAction::Bookmark);
            }
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.dialog = Some(Dialog::rename(&self.active_tab().name));
            }
            // less-style search.  `/` opens a forward prompt, `?` a
            // backward one.  `?` only reaches here as the post-shift
            // character: terminals sometimes report it with SHIFT,
            // sometimes with NONE — accept both.
            KeyEvent {
                code: KeyCode::Char('/'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let history = self.session.search_history.clone();
                self.dialog =
                    Some(Dialog::search(SearchDirection::Forward, history));
            }
            KeyEvent { code: KeyCode::Char('?'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                let history = self.session.search_history.clone();
                self.dialog =
                    Some(Dialog::search(SearchDirection::Backward, history));
            }
            // `n` repeats the last search in its stored direction; `N`
            // reverses it for one move (and does NOT update the stored
            // direction, matching less).
            KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                if let Some(last) = self.last_search.as_ref() {
                    self.step_search(last.direction);
                }
            }
            KeyEvent { code: KeyCode::Char('N'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                if let Some(last) = self.last_search.as_ref() {
                    self.step_search(last.direction.opposite());
                }
            }
            // Ctrl-T: open a fresh tab cloning the current filter and
            // immediately drop into the filter dialog so the user can
            // tailor it before any rendering surprises them.
            KeyEvent {
                code: KeyCode::Char('t'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                let cloned = self.active_stream().filter.clone();
                self.push_tab(TabKind::Stream, cloned);
                self.dialog =
                    Some(Dialog::filter(&self.active_stream().filter));
            }
            // `S`: open a Summary tab over the same filter the user
            // is already viewing.  Unlike Ctrl-T this does NOT drop
            // into the filter dialog: the user almost always wants a
            // histogram of "what I'm looking at right now", so we
            // skip the prompt and let `f` adjust afterwards.  Some
            // terminals report `S` with NONE, others with SHIFT;
            // accept both so the binding is robust across them.
            KeyEvent { code: KeyCode::Char('S'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                let cloned = self.active_stream().filter.clone();
                self.push_tab(TabKind::Summary, cloned);
            }
            // Tab cycles forward; Shift-Tab cycles back.  Some
            // terminals send Shift-Tab as `BackTab`, others as `Tab`
            // with the SHIFT modifier — accept both.
            KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.next_tab();
            }
            KeyEvent { code: KeyCode::BackTab, .. }
            | KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::SHIFT,
                ..
            } => {
                self.prev_tab();
            }
            // Ctrl-W: close active tab.  When a dialog is open this
            // arm doesn't fire (we returned earlier), so the dialog's
            // editor still sees Ctrl-W as kill-word.
            KeyEvent {
                code: KeyCode::Char('w'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.close_active_tab();
            }
            // `Y`: open the seeit-reproduction popup with the
            // command that would reproduce the active view.
            // Capital Y is unbound elsewhere; uppercase keeps it from
            // colliding with the lowercase `y` we may reserve for
            // yank-style actions later.  Terminals report `Y` with
            // either NONE or SHIFT — accept both.
            KeyEvent { code: KeyCode::Char('Y'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.open_seeit_command_dialog();
            }
            // Time-navigation step controls.  `=` and `+` grow the
            // step; `-` shrinks it.  `=` is the unshifted character on
            // US layouts (so the user can adjust without holding
            // Shift); `+` is accepted as an alias since it's the more
            // intuitive "increase" key — terminals report it with
            // either NONE or SHIFT, mirroring how `?`/`G`/`X`/`N` are
            // handled above.
            KeyEvent {
                code: KeyCode::Char('='),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.increase_time_step();
            }
            KeyEvent { code: KeyCode::Char('+'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.increase_time_step();
            }
            KeyEvent {
                code: KeyCode::Char('-'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.decrease_time_step();
            }
            // `>` advances by the current step; `<` rewinds.  Both are
            // shifted on US layouts; accept NONE or SHIFT (same
            // pattern as `?`/`G`).
            KeyEvent { code: KeyCode::Char('>'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.advance_time(Direction::Forward);
            }
            KeyEvent { code: KeyCode::Char('<'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.advance_time(Direction::Backward);
            }
            _ => {}
        }
    }
}

/// Single-line text editor used by both [`Dialog`] variants.  Owns the
/// buffer and cursor position and handles the readline-style keystrokes
/// the dialogs share; the dialog wrapping it is responsible for
/// Esc/Enter and any per-dialog side effects (e.g. parsing a filter).
struct LineEditor {
    /// Editable buffer, byte-indexed by [`Self::cursor`].
    text: String,
    /// Insertion point as a byte offset into `text`.  Always sits on a
    /// `char` boundary.
    cursor: usize,
}

/// Whether [`LineEditor::handle_edit`] consumed the keystroke.
enum EditAction {
    /// The key matched an editing binding (typing, motion, kill, etc.).
    /// The buffer/cursor may have changed; the dialog should refresh
    /// any derived state (such as the filter parse error).
    Handled,
    /// The key didn't match any editor binding.  The caller should
    /// interpret it (e.g. as Esc/Enter) or ignore it.
    Unhandled,
}

impl LineEditor {
    fn new(text: String) -> Self {
        let cursor = text.len();
        Self { text, cursor }
    }

    fn handle_edit(&mut self, key: KeyEvent) -> EditAction {
        match key {
            KeyEvent { code: KeyCode::Backspace, .. } => {
                self.backspace();
                EditAction::Handled
            }
            KeyEvent { code: KeyCode::Delete, .. } => {
                self.delete();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Left,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.move_left();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Right,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.move_right();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Home,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.cursor = 0;
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::End,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.cursor = self.text.len();
                EditAction::Handled
            }
            // Readline-style line editing.  ^A/^E mirror Home/End,
            // ^U kills to BOL, ^K kills to EOL, ^W kills the previous
            // whitespace-delimited word (matching shell behaviour, so a
            // whole `name=Nexus` token disappears at once), and Alt-B/
            // Alt-F move by alphanumeric word so the cursor can step
            // inside a token like `level>=warn`.
            KeyEvent {
                code: KeyCode::Char('a'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.cursor = 0;
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.cursor = self.text.len();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.kill_to_start();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Char('k'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.kill_to_end();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Char('w'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.kill_word_backward();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::ALT,
                ..
            } => {
                self.cursor = backward_word(&self.text, self.cursor);
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::ALT,
                ..
            } => {
                self.cursor = forward_word(&self.text, self.cursor);
                EditAction::Handled
            }
            // Plain typing: accept Char events with no modifiers other
            // than Shift (for capitals/symbols).  Anything with Ctrl/
            // Alt/Super is left to the caller.
            KeyEvent { code: KeyCode::Char(c), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                EditAction::Handled
            }
            _ => EditAction::Unhandled,
        }
    }

    fn kill_to_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.text.replace_range(0..self.cursor, "");
        self.cursor = 0;
    }

    fn kill_to_end(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.text.truncate(self.cursor);
    }

    fn kill_word_backward(&mut self) {
        let start = backward_whitespace_word(&self.text, self.cursor);
        if start == self.cursor {
            return;
        }
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
    }

    fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = prev_char_boundary(&self.text, self.cursor);
    }

    fn move_right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.cursor = next_char_boundary(&self.text, self.cursor);
    }
}

/// Browsing position within the search prompt's history.
///
/// `typed` records what the user had in the editor before they first
/// pressed Up — it's the prefix used to filter the history snapshot,
/// and what the editor is restored to when Down walks past the last
/// matching entry.  `pos` is an index into the dialog's `history`
/// vector; the editor's buffer always reflects `history[pos]` while a
/// nav is active.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchHistoryNav {
    typed: String,
    pos: usize,
}

/// All the data a freshly-created bookmark carries through the
/// name-dialog flow until the user commits it.  The cursor anchors
/// navigation; the display fields are cached so the Bookmarks tab can
/// render the row even when the source isn't currently loaded.
struct BookmarkDraft {
    cursor: Cursor,
    display_source: SourceId,
    display_time: chrono::DateTime<chrono::Utc>,
    display_name: String,
    display_msg: String,
}

/// Modal text dialog overlaying the main view.
///
/// Both variants share the same UX (Esc cancels, Enter applies) and
/// the same line-editing keys via [`LineEditor`].  The variants differ
/// in what "apply" means and whether there's parse feedback while
/// editing.
enum Dialog {
    /// Editing the active tab's [`Filter`].  Re-parses on every change
    /// so a parse error is visible live; Enter only commits when the
    /// buffer parses cleanly.
    Filter { editor: LineEditor, parse_error: Option<String> },
    /// Editing the active tab's display name.  Any string (including
    /// empty) is acceptable, so there's no parse feedback.
    Rename { editor: LineEditor },
    /// `less`-style regex search prompt.  `direction` is set by the
    /// key that opened the dialog (`/` → forward, `?` → backward).
    /// Re-compiles on every change so a regex parse error is visible
    /// live; Enter on a non-empty buffer commits, Enter on an empty
    /// buffer repeats the last search via [`App::repeat_last_search`].
    /// Up/Down walk the session's [`Session::search_history`] filtered
    /// by the prefix the user originally typed; see [`SearchHistoryNav`].
    Search {
        editor: LineEditor,
        direction: SearchDirection,
        parse_error: Option<String>,
        /// Snapshot of [`Session::search_history`] taken when the
        /// dialog was opened, most-recently-used first.  Copying it
        /// rather than borrowing keeps the dialog free of a `Session`
        /// reference.
        history: Vec<String>,
        /// `Some` while the user is browsing history (Up has been
        /// pressed at least once and the editor still shows a
        /// history entry); `None` while the editor shows freshly
        /// typed text.
        nav: Option<SearchHistoryNav>,
    },
    /// Naming a new bookmark just created via the `b` flow.  Carries
    /// the draft (cursor + display fields) the bookmark will anchor
    /// to.  Empty buffer → unnamed bookmark.  No parse feedback; any
    /// string is a valid name.
    BookmarkName { editor: LineEditor, draft: BookmarkDraft },
    /// Confirming the deletion of a bookmark from the Bookmarks tab.
    /// `id` identifies the bookmark; `label` is what to display in the
    /// dialog title.  No editor: the user picks Cancel (Esc) or
    /// Confirm (Enter).
    ConfirmDeleteBookmark { id: BookmarkId, label: String },
    /// Confirming a quit request triggered by `q` in the main or
    /// Bookmarks pane.  No editor: Esc cancels, Enter confirms.
    /// Esc and Ctrl-C are deliberately *not* bound to open this
    /// dialog at the top level — they're easy to hit by accident and
    /// an unwanted quit would tear down the user's in-flight viewport,
    /// search, and any unsaved exclude/include drafts.
    ConfirmQuit,
    /// Choosing which header columns to render: timestamp format,
    /// hostname mode, name, pid, and structured-fields visibility.
    /// Carries a draft [`RenderOpts`] that the user mutates with
    /// spacebar; Enter applies the draft to the active stream, Esc
    /// discards it.
    DisplayFields { draft: RenderOpts, cursor: usize },
    /// Read-only popup showing the `seeit` invocation that would
    /// reproduce the active view.  `Esc` or `Enter` close it.
    /// Opened by the `Y` binding.
    SeeitCommand { text: String },
    /// Read-only keybindings summary, organized by section.  Opened by
    /// the `h` binding (matching `less`).  `Esc`/`Enter` close it;
    /// `j`/`k`/Up/Down scroll when the content exceeds the popup
    /// height.  `scroll` is the index of the first body line drawn.
    Help { scroll: u16 },
}

/// One row in the [`Dialog::DisplayFields`] list.  Items are either
/// radio members of a group (timestamp format, hostname mode) or
/// independent checkboxes (pid, name, extras).  The flat ordering is
/// what `j`/`k` walk through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayFieldItem {
    /// short timestamp (no date) — radio with [`TimestampLong`].
    TimestampShort,
    /// date + ms timestamp — radio with [`TimestampShort`].
    TimestampLong,
    HostnameShort,
    HostnameFull,
    HostnameNone,
    Pid,
    Name,
    Extras,
}

const DISPLAY_FIELD_ITEMS: [DisplayFieldItem; 8] = [
    DisplayFieldItem::TimestampShort,
    DisplayFieldItem::TimestampLong,
    DisplayFieldItem::HostnameShort,
    DisplayFieldItem::HostnameFull,
    DisplayFieldItem::HostnameNone,
    DisplayFieldItem::Pid,
    DisplayFieldItem::Name,
    DisplayFieldItem::Extras,
];

impl DisplayFieldItem {
    /// Display label rendered next to the radio/checkbox glyph.
    fn label(self) -> &'static str {
        match self {
            Self::TimestampShort => "short timestamp (no date)",
            Self::TimestampLong => "full date and time",
            Self::HostnameShort => "short hostname",
            Self::HostnameFull => "full hostname",
            Self::HostnameNone => "no hostname",
            Self::Pid => "pid",
            Self::Name => "name",
            Self::Extras => "show all other fields",
        }
    }

    /// Returns true when this item terminates a logical group and a
    /// blank row should follow it in the rendered list, so the
    /// timestamp and hostname radio groups don't read as one
    /// five-option group.
    fn ends_group(self) -> bool {
        matches!(self, Self::TimestampLong | Self::HostnameNone)
    }

    /// Returns true iff this item represents a radio group member.
    /// Used by the renderer to choose between `(•)`/`( )` and
    /// `[x]`/`[ ]` glyphs.
    fn is_radio(self) -> bool {
        matches!(
            self,
            Self::TimestampShort
                | Self::TimestampLong
                | Self::HostnameShort
                | Self::HostnameFull
                | Self::HostnameNone
        )
    }

    /// Returns true iff this item is "selected" (radio) or "checked"
    /// (checkbox) under `opts`.
    fn is_active(self, opts: &RenderOpts) -> bool {
        match self {
            Self::TimestampShort => !opts.show_date,
            Self::TimestampLong => opts.show_date,
            Self::HostnameShort => opts.hostname == HostnameDisplay::Short,
            Self::HostnameFull => opts.hostname == HostnameDisplay::Full,
            Self::HostnameNone => opts.hostname == HostnameDisplay::None,
            Self::Pid => opts.show_pid,
            Self::Name => opts.show_name,
            Self::Extras => opts.show_extras,
        }
    }

    /// Applies this item's effect to `opts` when spacebar is pressed.
    /// Radio members set the group to their value (no-op when already
    /// selected); checkboxes flip in place.
    fn apply(self, opts: &mut RenderOpts) {
        match self {
            Self::TimestampShort => opts.show_date = false,
            Self::TimestampLong => opts.show_date = true,
            Self::HostnameShort => opts.hostname = HostnameDisplay::Short,
            Self::HostnameFull => opts.hostname = HostnameDisplay::Full,
            Self::HostnameNone => opts.hostname = HostnameDisplay::None,
            Self::Pid => opts.show_pid = !opts.show_pid,
            Self::Name => opts.show_name = !opts.show_name,
            Self::Extras => opts.show_extras = !opts.show_extras,
        }
    }
}

/// Outcome of one keystroke routed to the dialog.
enum DialogResult {
    /// Keep the dialog open with no further action.
    Stay,
    /// Close the dialog without changing app state.
    Cancel,
    /// Close the dialog and install this filter on the active tab.
    ApplyFilter(Filter),
    /// Close the dialog and rename the active tab to this string.
    ApplyRename(String),
    /// Close the dialog and install this regex as the active search.
    /// The regex is pre-compiled in the dialog so [`App`] doesn't have
    /// to handle a parse failure here.
    ApplySearch { pattern: String, regex: Regex, direction: SearchDirection },
    /// Close the dialog and repeat the most recent search in the given
    /// direction (Enter pressed with an empty buffer).  No-op if no
    /// previous search exists.
    RepeatSearch(SearchDirection),
    /// Close the dialog and add this bookmark to the session.  The
    /// bookmark's id and `created_at` are minted at apply time so they
    /// reflect the moment the user actually committed the name.
    ApplyBookmark { name: Option<BookmarkName>, draft: BookmarkDraft },
    /// Close the dialog and delete the bookmark with this id.  If the
    /// id is no longer present (concurrent edits aren't a thing here,
    /// but defending against stale state is cheap) the action is a
    /// no-op at the App layer.
    ApplyDeleteBookmark(BookmarkId),
    /// Close the dialog and tear down the TUI: the user confirmed the
    /// quit prompt.
    ApplyQuit,
    /// Close the dialog and install these [`RenderOpts`] on the active
    /// stream, repainting every tab targeting it.
    ApplyDisplayFields(RenderOpts),
}

impl Dialog {
    fn filter(current: &Filter) -> Self {
        let editor = LineEditor::new(current.to_string());
        let mut d = Self::Filter { editor, parse_error: None };
        d.reparse_filter();
        d
    }

    fn rename(current_name: &str) -> Self {
        Self::Rename { editor: LineEditor::new(current_name.to_string()) }
    }

    fn search(direction: SearchDirection, history: Vec<String>) -> Self {
        Self::Search {
            editor: LineEditor::new(String::new()),
            direction,
            parse_error: None,
            history,
            nav: None,
        }
    }

    /// Builds the bookmark-name dialog for a freshly-selected row.
    /// Empty initial buffer means "unnamed by default"; the user types
    /// to add a name.
    fn bookmark_name(draft: BookmarkDraft) -> Self {
        Self::BookmarkName { editor: LineEditor::new(String::new()), draft }
    }

    fn confirm_delete_bookmark(id: BookmarkId, label: String) -> Self {
        Self::ConfirmDeleteBookmark { id, label }
    }

    fn confirm_quit() -> Self {
        Self::ConfirmQuit
    }

    /// Builds the field-display dialog initialized with the active
    /// stream's current [`RenderOpts`].  Cursor starts at item 0
    /// (timestamp's "short" radio) — j/k navigate, spacebar mutates
    /// the draft, Enter applies, Esc cancels.
    fn display_fields(opts: RenderOpts) -> Self {
        Self::DisplayFields { draft: opts, cursor: 0 }
    }

    /// Read-only `seeit`-command popup carrying the prebuilt
    /// command string.  Built outside the dialog so the caller can
    /// fail closed (transient session, etc.) before opening any UI.
    fn seeit_command(text: String) -> Self {
        Self::SeeitCommand { text }
    }

    /// Read-only keybindings summary.  The body is built from
    /// [`HELP_SECTIONS`] at render time so adding a new section or
    /// binding is a one-liner there, not a constructor change here.
    fn help() -> Self {
        Self::Help { scroll: 0 }
    }

    fn editor(&self) -> Option<&LineEditor> {
        match self {
            Self::Filter { editor, .. }
            | Self::Rename { editor }
            | Self::Search { editor, .. }
            | Self::BookmarkName { editor, .. } => Some(editor),
            Self::ConfirmDeleteBookmark { .. }
            | Self::ConfirmQuit
            | Self::DisplayFields { .. }
            | Self::SeeitCommand { .. }
            | Self::Help { .. } => None,
        }
    }

    fn parse_error(&self) -> Option<&str> {
        match self {
            Self::Filter { parse_error, .. }
            | Self::Search { parse_error, .. } => parse_error.as_deref(),
            Self::Rename { .. }
            | Self::BookmarkName { .. }
            | Self::ConfirmDeleteBookmark { .. }
            | Self::ConfirmQuit
            | Self::DisplayFields { .. }
            | Self::SeeitCommand { .. }
            | Self::Help { .. } => None,
        }
    }

    fn title(&self) -> String {
        match self {
            Self::Filter { .. } => {
                "Filter (Esc cancel · Enter apply)".to_string()
            }
            Self::Rename { .. } => {
                "Rename tab (Esc cancel · Enter apply)".to_string()
            }
            Self::Search { .. } => {
                "Search (Esc cancel · Enter apply)".to_string()
            }
            Self::BookmarkName { .. } => {
                "Bookmark name (Esc cancel · Enter save · blank for \
                 unnamed)"
                    .to_string()
            }
            Self::ConfirmDeleteBookmark { label, .. } => {
                format!(
                    "Delete bookmark {label}? (Esc cancel · Enter \
                     confirm)"
                )
            }
            Self::ConfirmQuit => {
                "Quit? (Esc cancel · Enter confirm)".to_string()
            }
            Self::DisplayFields { .. } => {
                "Display fields (Esc cancel · Enter apply · space toggle)"
                    .to_string()
            }
            Self::SeeitCommand { .. } => {
                "seeit reproduction (Esc/Enter close)".to_string()
            }
            Self::Help { .. } => {
                "Keybindings (Esc/Enter close · j/k scroll)".to_string()
            }
        }
    }

    fn reparse_filter(&mut self) {
        if let Self::Filter { editor, parse_error } = self {
            *parse_error = match editor.text.parse::<Filter>() {
                Ok(_) => None,
                Err(e) => Some(e.to_string()),
            };
        }
    }

    fn reparse_search(&mut self) {
        if let Self::Search { editor, parse_error, .. } = self {
            *parse_error = if editor.text.is_empty() {
                None
            } else {
                match Regex::new(&editor.text) {
                    Ok(_) => None,
                    Err(e) => Some(regex_error_summary(&e)),
                }
            };
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> DialogResult {
        // Esc/Enter come first so they aren't shadowed by editor
        // bindings.  Anything not handled by either path is dropped.
        match key {
            KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                ..
            } => return DialogResult::Cancel,
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => return self.try_apply(),
            _ => {}
        }
        // The field-display dialog has its own keymap (j/k/Tab to move,
        // space to toggle, anything else dropped).  Handle it ahead of
        // the editor variants since it doesn't share their text-input
        // bindings.
        if let Self::DisplayFields { draft, cursor } = self {
            return handle_display_fields_key(draft, cursor, key);
        }
        // The help popup is scrollable (j/k/Up/Down) but otherwise has
        // no editor.  Same handle-it-before-the-editor-arm pattern as
        // DisplayFields.
        if let Self::Help { scroll } = self {
            return handle_help_key(scroll, key);
        }
        // Backspacing past the leading `/` (or `?`) of an empty search
        // prompt is treated as "I changed my mind" — the same as Esc.
        // The Filter, Rename, and BookmarkName dialogs deliberately
        // don't share this shortcut: their empty state is a meaningful
        // intermediate step (clear-and-retype), whereas an empty search
        // prompt is just the prompt sitting there with nothing to do.
        if let Self::Search { editor, .. } = self
            && editor.text.is_empty()
            && matches!(key.code, KeyCode::Backspace)
        {
            return DialogResult::Cancel;
        }
        // Up/Down walk the search-prompt history.  Routed here ahead of
        // the editor so the line editor's plain-text handler never sees
        // these keys — they're meaningful only on the search dialog.
        if let Self::Search { .. } = self {
            match key {
                KeyEvent {
                    code: KeyCode::Up,
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.search_history_back();
                    self.reparse_search();
                    return DialogResult::Stay;
                }
                KeyEvent {
                    code: KeyCode::Down,
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.search_history_forward();
                    self.reparse_search();
                    return DialogResult::Stay;
                }
                _ => {}
            }
        }
        let editor_result = match self {
            Self::Filter { editor, .. }
            | Self::Rename { editor }
            | Self::Search { editor, .. }
            | Self::BookmarkName { editor, .. } => editor.handle_edit(key),
            // Confirmation and selection dialogs have no editor;
            // non-Esc/Enter keys are dropped on the floor so a stray
            // `j`/`q` doesn't fall through to the underlying tab.
            Self::ConfirmDeleteBookmark { .. }
            | Self::ConfirmQuit
            | Self::SeeitCommand { .. } => {
                return DialogResult::Stay;
            }
            // Already handled above.
            Self::DisplayFields { .. } | Self::Help { .. } => {
                return DialogResult::Stay;
            }
        };
        if let EditAction::Handled = editor_result {
            // The user pressed a line-editor key while browsing
            // history; subsequent Up presses should treat the new
            // buffer as the prefix rather than continuing to walk
            // matches of the originally-typed text.
            if let Self::Search { nav, .. } = self {
                *nav = None;
            }
            self.reparse_filter();
            self.reparse_search();
        }
        DialogResult::Stay
    }

    /// Walks one step back through the search history (oldest
    /// direction).  No-op on non-Search dialogs and on Search dialogs
    /// whose history has no further entries matching the original
    /// prefix.
    fn search_history_back(&mut self) {
        let Self::Search { editor, history, nav, .. } = self else {
            return;
        };
        if history.is_empty() {
            return;
        }
        let (typed, start): (String, usize) = match nav {
            Some(n) => (n.typed.clone(), n.pos + 1),
            None => (editor.text.clone(), 0),
        };
        let found = history
            .iter()
            .enumerate()
            .skip(start)
            .find(|(_, p)| p.starts_with(&typed))
            .map(|(i, p)| (i, p.clone()));
        let Some((pos, pattern)) = found else {
            return;
        };
        *nav = Some(SearchHistoryNav { typed, pos });
        editor.text = pattern;
        editor.cursor = editor.text.len();
    }

    /// Walks one step forward through the search history (newer
    /// direction).  Past the most-recently-used match restores the
    /// editor to the prefix the user originally typed.  No-op on
    /// non-Search dialogs and on Search dialogs not currently
    /// browsing history.
    fn search_history_forward(&mut self) {
        let Self::Search { editor, history, nav, .. } = self else {
            return;
        };
        let Some(n) = nav else {
            return;
        };
        let typed = n.typed.clone();
        let pos = n.pos;
        let found = (0..pos)
            .rev()
            .find(|&i| history[i].starts_with(&typed))
            .map(|i| (i, history[i].clone()));
        match found {
            Some((new_pos, pattern)) => {
                *nav = Some(SearchHistoryNav { typed, pos: new_pos });
                editor.text = pattern;
                editor.cursor = editor.text.len();
            }
            None => {
                editor.text = typed;
                editor.cursor = editor.text.len();
                *nav = None;
            }
        }
    }

    fn try_apply(&mut self) -> DialogResult {
        match self {
            Self::Filter { editor, parse_error } => {
                match editor.text.parse::<Filter>() {
                    Ok(f) => DialogResult::ApplyFilter(f),
                    Err(e) => {
                        *parse_error = Some(e.to_string());
                        DialogResult::Stay
                    }
                }
            }
            Self::Rename { editor } => {
                DialogResult::ApplyRename(editor.text.clone())
            }
            Self::Search { editor, direction, parse_error, .. } => {
                if editor.text.is_empty() {
                    DialogResult::RepeatSearch(*direction)
                } else {
                    match Regex::new(&editor.text) {
                        Ok(regex) => DialogResult::ApplySearch {
                            pattern: editor.text.clone(),
                            regex,
                            direction: *direction,
                        },
                        Err(e) => {
                            *parse_error = Some(regex_error_summary(&e));
                            DialogResult::Stay
                        }
                    }
                }
            }
            Self::BookmarkName { editor, draft } => {
                let trimmed = editor.text.trim();
                let name = if trimmed.is_empty() {
                    None
                } else {
                    Some(BookmarkName::from(trimmed.to_string()))
                };
                DialogResult::ApplyBookmark {
                    name,
                    draft: BookmarkDraft {
                        cursor: draft.cursor.clone(),
                        display_source: draft.display_source.clone(),
                        display_time: draft.display_time,
                        display_name: draft.display_name.clone(),
                        display_msg: draft.display_msg.clone(),
                    },
                }
            }
            Self::ConfirmDeleteBookmark { id, .. } => {
                DialogResult::ApplyDeleteBookmark(*id)
            }
            Self::ConfirmQuit => DialogResult::ApplyQuit,
            Self::DisplayFields { draft, .. } => {
                DialogResult::ApplyDisplayFields(*draft)
            }
            // The seeit-command popup is read-only: Enter just
            // closes it, same as Esc.
            Self::SeeitCommand { .. } => DialogResult::Cancel,
            // Same story for the help popup.
            Self::Help { .. } => DialogResult::Cancel,
        }
    }
}

/// Routes a keystroke inside the field-display dialog.  Cursor moves
/// with `j`/`k`/Down/Up/Tab/BackTab and wraps; spacebar applies the
/// item under the cursor's effect on `draft`.  Anything else is
/// dropped (Esc/Enter are handled by [`Dialog::handle_key`] before
/// reaching here).
fn handle_display_fields_key(
    draft: &mut RenderOpts,
    cursor: &mut usize,
    key: KeyEvent,
) -> DialogResult {
    let n = DISPLAY_FIELD_ITEMS.len();
    match key {
        KeyEvent {
            code: KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
            ..
        } => {
            *cursor = (*cursor + 1) % n;
        }
        KeyEvent {
            code: KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab,
            modifiers: KeyModifiers::NONE,
            ..
        } => {
            *cursor = (*cursor + n - 1) % n;
        }
        KeyEvent {
            code: KeyCode::Char(' '),
            modifiers: KeyModifiers::NONE,
            ..
        } => {
            DISPLAY_FIELD_ITEMS[*cursor].apply(draft);
        }
        _ => {}
    }
    DialogResult::Stay
}

/// One section in the help popup.  The whole list of sections drives
/// both the rendered popup and the help-content tests, so adding a new
/// binding only requires a one-line entry here.
struct HelpSection {
    /// Section heading rendered above its items (e.g., "Navigation").
    title: &'static str,
    /// `(keys, description)` pairs.  `keys` is the left column shown to
    /// the user; multiple keys for the same action are joined with `,`.
    items: &'static [(&'static str, &'static str)],
}

/// All keybindings displayed by the help popup, in display order.
/// Bookmarks-pane bindings live in their own section since the active
/// keymap depends on which pane is focused.
const HELP_SECTIONS: &[HelpSection] = &[
    HelpSection {
        title: "Navigation",
        items: &[
            ("j, Down", "scroll down one line"),
            ("k, Up", "scroll up one line"),
            ("Space", "page down"),
            ("Ctrl-D", "half-page down"),
            ("Ctrl-U", "half-page up"),
            ("g, Home", "go to first record"),
            ("G, End", "go to last record"),
            ("<", "step back by current time step"),
            (">", "step forward by current time step"),
            ("-", "shrink time step"),
            ("=, +", "grow time step"),
        ],
    },
    HelpSection {
        title: "Search",
        items: &[
            ("/", "forward search"),
            ("?", "backward search"),
            ("n", "repeat last search"),
            ("N", "repeat last search, reversed"),
            ("Up, Down", "walk search history (at the search prompt)"),
        ],
    },
    HelpSection {
        title: "Filtering & selection",
        items: &[
            ("f", "edit filter"),
            ("x", "exclude rows matching the selected message"),
            ("X", "include only rows matching the selected message"),
            ("b", "bookmark a row"),
        ],
    },
    HelpSection {
        title: "Display",
        items: &[
            ("d", "display-fields dialog (timestamp, hostname, pid, …)"),
            ("F", "toggle structured-fields visibility"),
            ("D", "toggle date in timestamp"),
            ("R", "toggle raw rendering"),
            ("p", "toggle engine fetch-progress row"),
        ],
    },
    HelpSection {
        title: "Tabs",
        items: &[
            ("Tab", "next tab"),
            ("Shift-Tab", "previous tab"),
            ("Ctrl-T", "new tab (clone current filter)"),
            ("S", "new summary tab"),
            ("Ctrl-W", "close active tab"),
            ("r", "rename active tab"),
        ],
    },
    HelpSection {
        title: "Other",
        items: &[
            ("Y", "show `seeit` reproduction command"),
            ("h", "show this help"),
            ("q", "quit"),
        ],
    },
    HelpSection {
        title: "Bookmarks pane",
        items: &[
            ("j, k", "move cursor"),
            ("Enter", "open bookmark"),
            ("x", "delete bookmark (with confirmation)"),
        ],
    },
];

/// Routes a keystroke inside the help popup.  Scrolls with
/// `j`/`k`/Up/Down (one line) and Space/Ctrl-D/Ctrl-U/PageDown/PageUp
/// (page-ish jumps).  Anything else is dropped; Esc/Enter are handled
/// by [`Dialog::handle_key`] before reaching here.
///
/// We don't know the popup height here, so we don't clamp `scroll`
/// against the end of the body — the renderer does that on each frame
/// and skips drawing past the last line.  An overshoot here is
/// harmless and self-corrects on the next scroll-up.
fn handle_help_key(scroll: &mut u16, key: KeyEvent) -> DialogResult {
    let page = 10u16;
    match key {
        KeyEvent {
            code: KeyCode::Char('j') | KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            ..
        } => *scroll = scroll.saturating_add(1),
        KeyEvent {
            code: KeyCode::Char('k') | KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            ..
        } => *scroll = scroll.saturating_sub(1),
        KeyEvent {
            code: KeyCode::Char(' ') | KeyCode::PageDown,
            modifiers: KeyModifiers::NONE,
            ..
        } => *scroll = scroll.saturating_add(page),
        KeyEvent {
            code: KeyCode::Char('d'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => *scroll = scroll.saturating_add(page / 2),
        KeyEvent {
            code: KeyCode::Char('u'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => *scroll = scroll.saturating_sub(page / 2),
        KeyEvent {
            code: KeyCode::PageUp,
            modifiers: KeyModifiers::NONE,
            ..
        } => *scroll = scroll.saturating_sub(page),
        _ => {}
    }
    DialogResult::Stay
}

/// Builds the body of the help popup as a flat sequence of [`Line`]s
/// from [`HELP_SECTIONS`].  Section titles are emphasized; each item
/// is rendered as `<keys padded>  <description>` so the descriptions
/// line up.  A blank row separates sections.
///
/// The key-column width is computed from the longest key string across
/// every section so all descriptions line up consistently.
fn build_help_lines() -> Vec<Line<'static>> {
    let key_width = HELP_SECTIONS
        .iter()
        .flat_map(|s| s.items.iter())
        .map(|(k, _)| k.len())
        .max()
        .unwrap_or(0);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, section) in HELP_SECTIONS.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled(
            section.title,
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for (keys, desc) in section.items {
            let row = format!("  {:width$}  {}", keys, desc, width = key_width);
            lines.push(Line::raw(row));
        }
    }
    lines
}

/// First slice of a log message, suitable for the Bookmarks-tab row
/// preview and confirmation-dialog title.  Truncates at a generous
/// 80-character limit so a long `msg` doesn't blow out the dialog
/// width.
fn preview_msg(msg: &str) -> String {
    const LIMIT: usize = 80;
    if msg.len() <= LIMIT {
        return msg.to_string();
    }
    let mut end = LIMIT;
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = msg[..end].to_string();
    out.push('…');
    out
}

/// Compact a [`regex::Error`]'s display into a single line, since the
/// search prompt has at most one line of room for it.  All whitespace
/// runs (including the embedded newlines `regex` uses to point at the
/// offending character) collapse to single spaces.
fn regex_error_summary(e: &regex::Error) -> String {
    e.to_string().split_whitespace().collect::<Vec<_>>().join(" ")
}

fn prev_char_boundary(s: &str, byte_idx: usize) -> usize {
    s[..byte_idx].char_indices().next_back().map(|(i, _)| i).unwrap_or(0)
}

fn next_char_boundary(s: &str, byte_idx: usize) -> usize {
    s[byte_idx..]
        .chars()
        .next()
        .map(|c| byte_idx + c.len_utf8())
        .unwrap_or(byte_idx)
}

/// Readline `forward-word`: advance past any non-alphanumeric run, then
/// past the alphanumeric run, landing at the byte index just after the
/// next word.  Returns `s.len()` if there is no further word.
fn forward_word(s: &str, byte_idx: usize) -> usize {
    let bytes = s.as_bytes();
    let len = s.len();
    let mut i = byte_idx;
    while i < len && !bytes[i].is_ascii_alphanumeric() {
        i += 1;
    }
    while i < len && bytes[i].is_ascii_alphanumeric() {
        i += 1;
    }
    i
}

/// Readline `backward-word`: step back over any non-alphanumeric run,
/// then over the alphanumeric run, landing at the start of the previous
/// word.  Returns `0` if no earlier word exists.
fn backward_word(s: &str, byte_idx: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = byte_idx;
    while i > 0 && !bytes[i - 1].is_ascii_alphanumeric() {
        i -= 1;
    }
    while i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
        i -= 1;
    }
    i
}

/// Start of the previous whitespace-delimited word, used by ^W.  This
/// is more aggressive than [`backward_word`] — it treats anything
/// non-whitespace as part of the word, so a whole `name=Nexus` token
/// is killed in one shot.
fn backward_whitespace_word(s: &str, byte_idx: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = byte_idx;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    while i > 0 && !bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    i
}

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    // The bottom strip is normally one line (the footer).  When the
    // search dialog is open it expands by one to fit a parse error
    // beneath the prompt, mirroring the Filter dialog's two-line
    // popup but laid out inline at the screen bottom — closer to less.
    let bottom_height = match app.dialog.as_ref() {
        Some(Dialog::Search { parse_error: Some(_), .. }) => 2,
        _ => 1,
    };
    // Bookmarks pane has no parse activity to report, so we omit
    // both stats rows there and reclaim those lines for content.
    let stats_height: u16 = if app.bookmarks_active() { 0 } else { 1 };
    // The developer-only fetch-progress row is hidden by default and
    // toggled with `p`.  Suppressed in the Bookmarks pane (same
    // reasoning as `stats_height`).
    let fetch_stats_height: u16 =
        if !app.bookmarks_active() && app.show_fetch_stats { 1 } else { 0 };
    let [tabs_area, content_area, fetch_stats_area, stats_area, bottom_area] =
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(fetch_stats_height),
            Constraint::Length(stats_height),
            Constraint::Length(bottom_height),
        ])
        .areas(area);

    render_tab_bar(frame, app, tabs_area);

    let new_h = content_area.height;
    let new_w = content_area.width;
    // Record a debounced mutation when the terminal size changes, so
    // the next due() check picks up the resize.  The initial 0→N
    // transition at first render counts as a resize too; the
    // resulting flush is a no-op write 10s later and not worth the
    // extra guard.
    if app.viewport_height != new_h || app.viewport_width != new_w {
        app.policy.record(Cadence::Debounced);
    }
    app.viewport_height = new_h;
    app.viewport_width = new_w;

    if app.bookmarks_active() {
        render_bookmarks_pane(frame, app, content_area);
        render_bookmarks_footer(frame, app, bottom_area);
        if let Some(
            d @ (Dialog::ConfirmDeleteBookmark { .. }
            | Dialog::ConfirmQuit
            | Dialog::SeeitCommand { .. }
            | Dialog::Help { .. }),
        ) = app.dialog.as_ref()
        {
            render_dialog(frame, d, area);
        }
        return;
    }

    // Re-clamp in case the viewport just shrank past the previous top.
    let max_top =
        app.active_tab().max_top(app.viewport_height, app.viewport_width);
    if app.active_tab().viewport_top > max_top {
        app.active_tab_mut().viewport_top = max_top;
    }

    let tab = app.active_tab();
    let total = tab.formatted().len();
    let top = tab.viewport_top.get();
    // Walk forward from `top`, accumulating each line's wrapped row
    // count until we've covered the content area.  The result is the
    // first logical line index that no longer fits — the slice
    // `[top..bottom]` is what we hand to the Paragraph (ratatui
    // clips the last partial visual line if any).
    let max_visual = content_area.height as usize;
    let mut bottom = top;
    let mut visual_used: usize = 0;
    while bottom < total && visual_used < max_visual {
        let rows =
            visual_rows_for(&tab.formatted()[bottom], content_area.width);
        visual_used = visual_used.saturating_add(rows);
        bottom += 1;
    }

    // "End of stream" surfaces when the viewport's last visible line
    // is the last line we have AND no more records can appear past
    // it.  A streamview-backed tab (the live case) checks
    // `is_forward_eof`; test fixtures without a streamview have all
    // their data already materialized, so reaching the bottom is
    // sufficient.  Scrolling up makes `bottom < total` and the
    // indicator disappears.  Shown on the always-on user status line
    // (rather than the keybinding footer, which is too long to keep
    // visible on narrow terminals) so users actually see it.
    let at_eof = total > 0
        && bottom == total
        && tab.viewport.as_ref().is_none_or(|v| v.is_forward_eof());

    // While a long op targeting the active tab is in flight, the
    // progress bar replaces the user status line.  After completion,
    // the user status returns on the next frame.  The BOF/EOF markers
    // are only appended outside the long-op branch — a running op is
    // by definition not "done", so the marker would be misleading.
    let active_filter = app.active_stream().filter.clone();
    // XXX-dap
    // let user_status_text = match app.long_op.as_ref() {
    //     Some(op) if op.targets_tab(TabIdx(app.active)) => {
    //         format_long_op_progress(op, stats_area.width.into())
    //     }
    //     _ => format_user_status(
    //         tab,
    //         &app.engine,
    //         &active_filter,
    //         top,
    //         bottom,
    //         at_eof,
    //     ),
    // };
    let user_status_text = format_user_status(
        tab,
        &app.engine,
        &active_filter,
        top,
        bottom,
        at_eof,
    );
    frame.render_widget(Paragraph::new(user_status_text), stats_area);

    // Optional developer-oriented fetch-progress row, toggled by `p`.
    // Always shows the engine's running fetch totals, even during a
    // long op — the user status above is already commandeered by the
    // long-op progress bar, and seeing the fetch counters mid-op is
    // the whole reason a developer reaches for this toggle.
    if fetch_stats_height > 0 {
        let fetch_text = format_fetch_stats(tab.parse_stats());
        frame.render_widget(Paragraph::new(fetch_text), fetch_stats_area);
    }

    let selected_event = tab.select.map(|s| s.event_idx);
    // In raw mode, pre-wrap each logical line at the column boundary
    // ourselves so the rendered visual rows are contiguous slices of
    // the source.  That lets the user copy a wrapped line out and
    // strip newlines to recover the original bytes; word-wrap's
    // newline-replaces-space behavior would lose that property.
    // Non-raw mode keeps ratatui's word-wrap, which reads better for
    // header-and-extras layouts.
    let raw_mode = app.active_stream().show_raw;
    let lines: Vec<Line<'_>> = tab.formatted()[top..bottom]
        .iter()
        .enumerate()
        .flat_map(|(i, s)| {
            let line_index = top + i;
            // Highlight every display line that belongs to the
            // selected record so users see the full record they're
            // about to exclude/include/bookmark, not just its header
            // row.  Distinct from the search highlight (REVERSED on
            // matched runs); a row-wide background reads as "this is
            // the entry you're about to act on" without fighting
            // search styling.
            let selected = selected_event.is_some_and(|target| {
                tab.event_for_line().get(line_index).copied() == Some(target)
            });
            let selected_style = Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD);
            let chunks: Vec<&str> = if raw_mode {
                column_chunks(s, content_area.width)
            } else {
                vec![s.as_str()]
            };
            chunks
                .into_iter()
                .map(|chunk| {
                    let mut line = match &tab.search {
                        Some(search) => highlight_line(chunk, &search.regex),
                        None => Line::raw(chunk),
                    };
                    if selected {
                        line = line.style(selected_style);
                    }
                    line
                })
                .collect::<Vec<_>>()
        })
        .collect();
    // In non-raw mode, let ratatui word-wrap each logical line.
    // `trim: false` keeps the leading whitespace on `    key = value`
    // extras lines so wraps don't collapse indentation.  Raw mode
    // already pre-wrapped above, so each Line is one terminal row.
    let mut paragraph = Paragraph::new(lines);
    if !raw_mode {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }
    frame.render_widget(paragraph, content_area);

    // Bottom strip: search prompt or footer, never both.
    match app.dialog.as_ref() {
        Some(Dialog::Search { editor, direction, parse_error, .. }) => {
            render_search_prompt(
                frame,
                editor,
                *direction,
                parse_error.as_deref(),
                bottom_area,
            );
        }
        _ => {
            // Footer hints prioritize the actions a fresh user needs to
            // discover.  `n`/`N` repeat searches and are taught by the
            // `/` workflow, so they're omitted here to keep the live
            // footer readable on 80-column terminals.
            let footer = if let Some(notice) = app.notice.as_deref() {
                notice.to_string()
            } else if let Some(sel) = tab.select {
                let entry_total = tab.events().len();
                match sel.action {
                    SelectionAction::Exclude | SelectionAction::Include => {
                        let verb = match sel.action {
                            SelectionAction::Exclude => "exclude",
                            SelectionAction::Include => "include",
                            SelectionAction::Bookmark => unreachable!(),
                        };
                        format!(
                            "{verb}: j/k select · Enter {verb} msg · \
                             Esc cancel · entry {}/{}",
                            sel.event_idx + 1,
                            entry_total,
                        )
                    }
                    SelectionAction::Bookmark => format!(
                        "bookmark: j/k select · Enter name · \
                         Esc cancel · entry {}/{}",
                        sel.event_idx + 1,
                        entry_total,
                    ),
                }
            } else if tab.kind == TabKind::Summary {
                // Summary tabs show histogram rows, not records: the
                // exclude/include/bookmark/time-step keys would all be
                // no-ops, and `F fields=` toggles extras for events
                // that this tab doesn't display.  Hide them from the
                // footer to avoid teaching the user actions that don't
                // apply.
                if total == 0 {
                    "q quit · h help · 0/0".to_string()
                } else {
                    format!(
                        "q quit · h help · {}-{} of {}",
                        top + 1,
                        bottom,
                        total,
                    )
                }
            } else if total == 0 {
                format!(
                    "q quit · h help · f filter · F fields={} · \
                     d display options · </> step={} · \
                     x/X exclude/include · b bookmark · ^T new · \
                     S summary · ^W close · r rename",
                    if app.active_stream().show_extras { "on" } else { "off" },
                    app.current_step_label(),
                )
            } else {
                format!(
                    "q quit · h help · f filter · F fields={} · \
                     d display options · </> step={} · \
                     ^T new · ^W close",
                    if app.active_stream().show_extras { "on" } else { "off" },
                    app.current_step_label(),
                )
            };
            frame.render_widget(Paragraph::new(footer), bottom_area);
        }
    }

    // Centered popups (Filter, Rename, BookmarkName,
    // ConfirmDeleteBookmark, ConfirmQuit, DisplayFields,
    // SeeitCommand, Help) draw on top of the rest.  The Search prompt
    // is laid out inline above and is skipped here.
    if let Some(
        dialog @ (Dialog::Filter { .. }
        | Dialog::Rename { .. }
        | Dialog::BookmarkName { .. }
        | Dialog::ConfirmDeleteBookmark { .. }
        | Dialog::ConfirmQuit
        | Dialog::DisplayFields { .. }
        | Dialog::SeeitCommand { .. }
        | Dialog::Help { .. }),
    ) = app.dialog.as_ref()
    {
        render_dialog(frame, dialog, area);
    }
}

/// Width (in display columns) of a timestamp rendered by
/// [`seer::format_time`] with [`seer::TimestampDisplay::DateAndTime`] —
/// `2026-04-30T15:30:00.743Z`.  Used to reserve the leftmost column of
/// each Bookmarks-tab entry.
const BOOKMARK_TS_WIDTH: usize = 24;

/// Inter-column separator on Bookmarks-tab rows.  Middle-dot is one
/// column wide, so the literal is three display columns.
const BOOKMARK_COL_SEP: &str = " · ";
const BOOKMARK_COL_SEP_WIDTH: usize = 3;

/// Renders the Bookmarks pane.
///
/// Each entry occupies a variable number of rows: column 1 is the
/// bookmarked event's timestamp, column 2 is the user-given bookmark
/// name (if any), column 3 is the bunyan `name` + message preview.
/// Columns 2 and 3 wrap independently within their share of the pane
/// width; the entry's height is the taller column.  A final indented
/// line under column 3 shows when the bookmark was created.  The
/// selected entry highlights every row it occupies.
///
/// The pane has no scrolling yet: entries past the visible height are
/// truncated.
fn render_bookmarks_pane(frame: &mut Frame, app: &App, area: Rect) {
    let bookmarks = app.flat_bookmarks();
    if bookmarks.is_empty() {
        // Should be unreachable because `bookmarks_active()` requires
        // `has_bookmarks_tab()` which requires count > 0.  Defensive
        // empty state instead of a panic.
        frame.render_widget(Paragraph::new(Line::raw("(no bookmarks)")), area);
        return;
    }
    let cursor_id = app.bookmark_cursor;
    let total_w = area.width as usize;
    let overhead = BOOKMARK_TS_WIDTH + 2 * BOOKMARK_COL_SEP_WIDTH;

    // Degenerate-narrow pane: collapse to a single-line-per-entry
    // fallback so the user still sees their bookmarks rather than
    // nothing.  Picks an arbitrary minimum that leaves at least two
    // characters in each of the two wrappable columns.
    if total_w < overhead + 4 {
        let lines: Vec<Line<'_>> = bookmarks
            .iter()
            .take(area.height as usize)
            .map(|bm| compact_bookmark_line(bm, cursor_id))
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    let rest = total_w - overhead;
    let name_col_w = rest / 2;
    let msg_col_w = rest - name_col_w;
    let cap = area.height as usize;
    let mut lines: Vec<Line<'_>> = Vec::new();

    for bm in bookmarks {
        if lines.len() >= cap {
            break;
        }
        let user_name =
            bm.name.as_ref().map(BookmarkName::to_string).unwrap_or_default();
        let name_rows = wrap_to_width(&user_name, name_col_w);
        // App name and message share column 3.  Glue them with `: ` so
        // an empty name doesn't leave a dangling separator; an empty
        // msg likewise just shows the name.
        let app_and_msg =
            match (bm.display_name.is_empty(), bm.display_msg.is_empty()) {
                (true, true) => String::new(),
                (true, false) => bm.display_msg.clone(),
                (false, true) => bm.display_name.clone(),
                (false, false) => {
                    format!("{}: {}", bm.display_name, bm.display_msg)
                }
            };
        let msg_rows = wrap_to_width(&app_and_msg, msg_col_w);
        let created_line = format!(
            "bookmark created at {}",
            seer::format_time(
                &bm.created_at,
                seer::TimestampDisplay::DateAndTime
            ),
        );
        let total_rows = name_rows.len().max(msg_rows.len() + 1);
        let ts_str = seer::format_time(
            &bm.display_time,
            seer::TimestampDisplay::DateAndTime,
        );
        let highlighted = Some(bm.id) == cursor_id;

        for row_idx in 0..total_rows {
            if lines.len() >= cap {
                break;
            }
            let ts_cell = if row_idx == 0 { ts_str.as_str() } else { "" };
            let name_cell =
                name_rows.get(row_idx).map(String::as_str).unwrap_or("");
            let msg_cell: String = if row_idx < msg_rows.len() {
                msg_rows[row_idx].clone()
            } else if row_idx == msg_rows.len() {
                created_line.clone()
            } else {
                String::new()
            };
            let row_text = format!(
                "{ts:<ts_w$}{sep}{name:<name_w$}{sep}{msg}",
                ts = ts_cell,
                name = name_cell,
                msg = msg_cell,
                ts_w = BOOKMARK_TS_WIDTH,
                name_w = name_col_w,
                sep = BOOKMARK_COL_SEP,
            );
            let mut line = Line::raw(row_text);
            if highlighted {
                line = line.style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
            }
            lines.push(line);
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// Single-line bookmark row used when the pane is too narrow for the
/// three-column layout.  Renders just `<time> · <msg>` so the user
/// still sees enough to recognize each entry.
fn compact_bookmark_line<'a>(
    bm: &'a Bookmark,
    cursor_id: Option<BookmarkId>,
) -> Line<'a> {
    let row = format!(
        "{} · {}",
        seer::format_time(
            &bm.display_time,
            seer::TimestampDisplay::DateAndTime
        ),
        bm.display_msg,
    );
    let mut line = Line::raw(row);
    if Some(bm.id) == cursor_id {
        line = line.style(
            Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD),
        );
    }
    line
}

/// Wraps `text` to fit `width` display columns.
///
/// Breaks at whitespace when possible; if a single word exceeds
/// `width` it is split at column boundaries via [`column_chunks`] so
/// long unbroken tokens (e.g. UUIDs) don't overflow the column.  Empty
/// input returns one empty line so callers always have at least one
/// row to render against.  `width == 0` (degenerate column) returns
/// one empty line.
fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;
    for word in text.split_whitespace() {
        let word_chars = word.chars().count();
        if word_chars > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_chars = 0;
            }
            for chunk in column_chunks(word, width as u16) {
                lines.push(chunk.to_string());
            }
            continue;
        }
        let space = usize::from(current_chars > 0);
        if current_chars + space + word_chars > width {
            lines.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        if current_chars > 0 {
            current.push(' ');
            current_chars += 1;
        }
        current.push_str(word);
        current_chars += word_chars;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn render_bookmarks_footer(frame: &mut Frame, app: &App, area: Rect) {
    let count = app.session.bookmark_count();
    let footer = if let Some(notice) = app.notice.as_deref() {
        notice.to_string()
    } else {
        format!(
            "q quit · j/k select · Enter open · x delete · Y seeit · \
             Tab cycle · h help · {count} bookmark{}",
            if count == 1 { "" } else { "s" },
        )
    };
    frame.render_widget(Paragraph::new(footer), area);
}

/// Splits `text` into [`Span`]s, highlighting every (non-empty) match
/// of `regex` with `Modifier::REVERSED`.  Zero-width matches (e.g.
/// from `a*`) are skipped so they don't multiply into a sea of empty
/// styled spans.
fn highlight_line<'a>(text: &'a str, regex: &Regex) -> Line<'a> {
    let mut spans: Vec<Span<'a>> = Vec::new();
    let mut last_end = 0;
    for m in regex.find_iter(text) {
        if m.start() == m.end() {
            continue;
        }
        if m.start() > last_end {
            spans.push(Span::raw(&text[last_end..m.start()]));
        }
        spans.push(Span::styled(
            &text[m.start()..m.end()],
            Style::default().add_modifier(Modifier::REVERSED),
        ));
        last_end = m.end();
    }
    if last_end < text.len() {
        spans.push(Span::raw(&text[last_end..]));
    }
    Line::from(spans)
}

/// Renders a `less`-style search prompt (`/foo` or `?foo`) on the top
/// row of `area`, with the cursor positioned after the typed text.
/// If `parse_error` is set and `area` is at least two rows tall, the
/// error is drawn in red on the row below.
fn render_search_prompt(
    frame: &mut Frame,
    editor: &LineEditor,
    direction: SearchDirection,
    parse_error: Option<&str>,
    area: Rect,
) {
    let prompt_area = Rect::new(area.x, area.y, area.width, 1);
    let prefix = direction.prompt();
    let prompt_text = format!("{prefix}{}", editor.text);
    frame.render_widget(Paragraph::new(Line::raw(prompt_text)), prompt_area);

    // Cursor sits at: x + 1 (for the prefix char) + cursor offset.
    // Search patterns are ASCII in practice, so byte offset == column.
    let col = prompt_area
        .x
        .saturating_add(1)
        .saturating_add(u16::try_from(editor.cursor).unwrap_or(u16::MAX));
    let col = col.min(prompt_area.x.saturating_add(prompt_area.width));
    frame.set_cursor_position(Position::new(col, prompt_area.y));

    if let Some(err) = parse_error
        && area.height > 1
    {
        let err_area = Rect::new(area.x, area.y + 1, area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::raw(err)).style(Style::new().fg(Color::Red)),
            err_area,
        );
    }
}

fn render_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mut titles: Vec<Line<'_>> =
        app.tabs.iter().map(|t| Line::raw(t.name.as_str())).collect();
    if app.has_bookmarks_tab() {
        titles.push(Line::raw("Bookmarks"));
    }
    let widget = Tabs::new(titles)
        .select(app.active)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(widget, area);
}

/// Carves a centered popup over `area` and draws the appropriate
/// dialog body.
///
/// Variants with an editor (Filter/Rename/Search/BookmarkName) render
/// their text and cursor on the first row; Filter/Search additionally
/// render any parse error in red below.  ConfirmDeleteBookmark and
/// ConfirmQuit have no editor and show only the question encoded in
/// their title.  DisplayFields is rendered separately
/// ([`render_display_fields_dialog`]) since its body is a list of
/// rows, not an editor + error.
fn render_dialog(frame: &mut Frame, dialog: &Dialog, area: Rect) {
    if let Dialog::DisplayFields { draft, cursor } = dialog {
        render_display_fields_dialog(frame, dialog, draft, *cursor, area);
        return;
    }
    if let Dialog::SeeitCommand { text } = dialog {
        render_seeit_command_dialog(frame, dialog, text, area);
        return;
    }
    if let Dialog::Help { scroll } = dialog {
        render_help_dialog(frame, dialog, *scroll, area);
        return;
    }

    // Wrap the editor text so a long filter doesn't get clipped at the
    // popup's right edge.  Compute the inner width from the same
    // 70%-of-area-width that `popup_area` uses (minus 2 for the
    // borders) and grow the popup's height to fit the wrapped lines.
    // `popup_area` clamps the height back down if it would overflow the
    // screen, so extremely long text is still safe — it just truncates
    // at the bottom of the available area rather than overflowing.
    let popup_width = area.width.saturating_mul(70) / 100;
    let inner_width = (popup_width.saturating_sub(2) as usize).max(1);
    let edit_rows = dialog
        .editor()
        .map(|e| e.text.len().div_ceil(inner_width).max(1))
        .unwrap_or(1);
    let edit_rows = u16::try_from(edit_rows).unwrap_or(u16::MAX);
    // Two rows reserved below the editor for parse-error display (or
    // blank padding for dialogs without one), matching the original
    // fixed-height layout so single-line dialogs look unchanged.
    let popup_height = edit_rows.saturating_add(4);

    let popup = popup_area(area, 70, popup_height);
    // Clear the underlying rows so the editor isn't drawn on top of
    // them.
    frame.render_widget(Clear, popup);

    let block = Block::bordered().title(dialog.title());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [edit_area, error_area] =
        Layout::vertical([Constraint::Length(edit_rows), Constraint::Min(0)])
            .areas(inner);

    if let Some(editor) = dialog.editor() {
        // Split the text into chunks of `inner_width` columns so the
        // cursor math below stays in lockstep with what the user sees.
        // The dialog buffers are ASCII in practice (filter syntax is
        // ASCII, tab names and bookmark names typically too), so the
        // byte offset doubles as the column.  If we ever accept
        // multibyte chars, both this chunking and the cursor
        // calculation need grapheme-aware replacements.
        let lines = wrap_dialog_text(&editor.text, inner_width);
        frame.render_widget(Paragraph::new(lines), edit_area);

        // When the cursor sits exactly on a wrap boundary at the end
        // of the buffer (e.g. typing the Nth char on a width-N line),
        // draw it at the right edge of the previous row instead of
        // column 0 of an empty next row — matches typical editor UX
        // and keeps the cursor visible without allocating an extra
        // wrapped line just for it.
        let (cursor_row, cursor_col) = if editor.cursor > 0
            && editor.cursor == editor.text.len()
            && editor.cursor.is_multiple_of(inner_width)
        {
            (editor.cursor / inner_width - 1, inner_width)
        } else {
            (editor.cursor / inner_width, editor.cursor % inner_width)
        };
        let row = edit_area
            .y
            .saturating_add(u16::try_from(cursor_row).unwrap_or(u16::MAX))
            .min(
                edit_area.y.saturating_add(edit_area.height.saturating_sub(1)),
            );
        let col = edit_area
            .x
            .saturating_add(u16::try_from(cursor_col).unwrap_or(u16::MAX))
            .min(edit_area.x.saturating_add(edit_area.width));
        frame.set_cursor_position(Position::new(col, row));
    }

    if let Some(err) = dialog.parse_error()
        && error_area.height > 0
    {
        frame.render_widget(
            Paragraph::new(Line::raw(err)).style(Style::new().fg(Color::Red)),
            error_area,
        );
    }
}

/// Splits `text` into [`Line`]s no wider than `width` columns, walking
/// back to the nearest `char` boundary so multibyte UTF-8 sequences
/// aren't split.  An empty input still yields one (empty) line so the
/// caller's cursor has a row to land on.
fn wrap_dialog_text(text: &str, width: usize) -> Vec<Line<'_>> {
    if text.is_empty() {
        return vec![Line::raw("")];
    }
    let mut lines = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + width).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            // `width` is smaller than the next char's UTF-8 length.
            // Advance by one char so we don't loop forever; this is a
            // degenerate case for ASCII content with `width >= 1`.
            end = start
                + text[start..].chars().next().expect("non-empty").len_utf8();
        }
        lines.push(Line::raw(&text[start..end]));
        start = end;
    }
    lines
}

/// Draws the field-display dialog: one row per item in
/// [`DISPLAY_FIELD_ITEMS`], with a `(•)`/`( )` glyph for radio members
/// and `[x]`/`[ ]` for checkboxes.  A blank row separates the
/// timestamp, hostname, and checkbox groups so the radio groups don't
/// read as one combined five-option list.  The row under the cursor
/// is highlighted with `Modifier::REVERSED`.
fn render_display_fields_dialog(
    frame: &mut Frame,
    dialog: &Dialog,
    draft: &RenderOpts,
    cursor: usize,
    area: Rect,
) {
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(
        DISPLAY_FIELD_ITEMS.len() + 2, // 2 separators between 3 groups
    );
    for (i, item) in DISPLAY_FIELD_ITEMS.iter().enumerate() {
        let glyph = match (item.is_radio(), item.is_active(draft)) {
            (true, true) => "(•)",
            (true, false) => "( )",
            (false, true) => "[x]",
            (false, false) => "[ ]",
        };
        let text = format!("{glyph} {}", item.label());
        let line = Line::raw(text);
        let line = if i == cursor {
            line.style(Style::default().add_modifier(Modifier::REVERSED))
        } else {
            line
        };
        lines.push(line);
        if item.ends_group() {
            lines.push(Line::raw(""));
        }
    }
    // Outer border adds 2 rows of frame; the 50-column width
    // comfortably fits the longest label ("short timestamp (no date)").
    let body_height = u16::try_from(lines.len()).expect("fits in u16") + 2;
    let popup = popup_area(area, 50, body_height);
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(dialog.title());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Renders the read-only `seeit`-command popup.  The body wraps the
/// command at the popup's inner width and shows it with no cursor;
/// `wrap_dialog_text` is shared with the editor variants so a long
/// command (rare, but possible) flows across multiple rows the same
/// way a long filter does.
fn render_seeit_command_dialog(
    frame: &mut Frame,
    dialog: &Dialog,
    text: &str,
    area: Rect,
) {
    let popup_width = area.width.saturating_mul(70) / 100;
    let inner_width = (popup_width.saturating_sub(2) as usize).max(1);
    let wrapped = wrap_dialog_text(text, inner_width);
    // One row per wrapped line plus the two-row outer frame.  Clamp
    // via `popup_area`'s height-min so an extremely long command
    // still fits the available screen.
    let body_rows = u16::try_from(wrapped.len().max(1)).unwrap_or(u16::MAX);
    let popup_height = body_rows.saturating_add(2);
    let popup = popup_area(area, 70, popup_height);
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(dialog.title());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(wrapped), inner);
}

/// Renders the read-only help popup.  The body comes from
/// [`build_help_lines`] and is sliced by `scroll` so long content can
/// be paged with `j`/`k`/Space.
///
/// The popup width is the longest body line (or the title, whichever
/// is wider) plus the 2-cell border, clamped to the available area —
/// so descriptions don't clip on an 80-column terminal but the popup
/// doesn't sprawl wider than the content needs.
fn render_help_dialog(
    frame: &mut Frame,
    dialog: &Dialog,
    scroll: u16,
    area: Rect,
) {
    let lines = build_help_lines();
    let total_rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);

    let title = dialog.title();
    let widest = lines.iter().map(line_width).max().unwrap_or(0);
    let needed = widest.max(title.chars().count()).saturating_add(2);
    let popup_width = u16::try_from(needed).unwrap_or(u16::MAX);
    let preferred_height = total_rows.saturating_add(2);
    let popup_height = preferred_height.min(area.height);
    let popup = popup_area_with_width(area, popup_width, popup_height);

    let block = Block::bordered().title(title);
    let inner_height = popup.height.saturating_sub(2);
    let max_scroll = total_rows.saturating_sub(inner_height);
    let scroll = scroll.min(max_scroll);
    let start = scroll as usize;
    let end = (start + inner_height as usize).min(lines.len());
    let visible: Vec<Line<'static>> = lines[start..end].to_vec();

    frame.render_widget(Clear, popup);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(visible), inner);
}

/// Sum of every span's column width on `line`.  The help-popup
/// content is ASCII, so chars equal columns.
fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Like [`popup_area`] but takes the width in cells rather than a
/// percentage.  Used by [`render_help_dialog`] where the width is
/// derived (80%) but we want to share the centering logic.
fn popup_area_with_width(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

fn popup_area(area: Rect, percent_width: u16, height: u16) -> Rect {
    let width = area.width.saturating_mul(percent_width) / 100;
    let height = height.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn shift(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn back_tab() -> KeyEvent {
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)
    }

    fn app(rows: usize, height: u16) -> App {
        let rows = (0..rows).map(|i| format!("row {i}")).collect();
        let mut a = App::with_rows(rows);
        a.viewport_height = height;
        a
    }

    /// Drives the open dialog through a sequence of typed characters.
    /// Panics if any keystroke unexpectedly closes the dialog.
    fn type_into(d: &mut Dialog, s: &str) {
        for c in s.chars() {
            match d.handle_key(key(KeyCode::Char(c))) {
                DialogResult::Stay => {}
                _ => panic!("typing {c:?} unexpectedly closed dialog"),
            }
        }
    }

    // ---------- top-level (no dialog) ----------

    #[test]
    fn q_opens_quit_confirmation() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('q')));
        assert!(!a.quit);
        assert!(matches!(a.dialog, Some(Dialog::ConfirmQuit)));
    }

    #[test]
    fn esc_does_not_open_quit_confirmation() {
        // Esc is reserved for cancelling whatever's in front of the
        // user (a dialog, an exclude/include selection); pressing it
        // at the main view should be a no-op rather than an
        // accidental quit prompt.
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Esc));
        assert!(!a.quit);
        assert!(a.dialog.is_none());
    }

    #[test]
    fn ctrl_c_does_not_open_quit_confirmation() {
        // Ctrl-C is muscle-memory for "interrupt" in a terminal;
        // letting it tear down the TUI's in-flight state would be a
        // common foot-gun.  `q` is the only quit-prompt key.
        let mut a = app(10, 5);
        a.handle_key(ctrl('c'));
        assert!(!a.quit);
        assert!(a.dialog.is_none());
    }

    #[test]
    fn quit_confirmation_enter_quits() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('q')));
        a.handle_key(key(KeyCode::Enter));
        assert!(a.quit);
        assert!(a.dialog.is_none());
    }

    #[test]
    fn quit_confirmation_esc_cancels() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('q')));
        a.handle_key(key(KeyCode::Esc));
        assert!(!a.quit);
        assert!(a.dialog.is_none());
    }

    #[test]
    fn quit_confirmation_drops_other_keys() {
        // Inside the confirm dialog, stray keys must not fall through
        // to scroll the underlying tab or quit unilaterally.
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('q')));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('q')));
        assert!(!a.quit);
        assert_eq!(a.active_tab().viewport_top, 0);
        assert!(matches!(a.dialog, Some(Dialog::ConfirmQuit)));
    }

    // ---------- scrolling ----------

    #[test]
    fn j_and_down_scroll_down_one() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.active_tab().viewport_top, 1);
        a.handle_key(key(KeyCode::Down));
        assert_eq!(a.active_tab().viewport_top, 2);
    }

    #[test]
    fn k_and_up_scroll_up_one() {
        let mut a = app(10, 5);
        a.active_tab_mut().viewport_top = LineIdx(3);
        a.handle_key(key(KeyCode::Char('k')));
        assert_eq!(a.active_tab().viewport_top, 2);
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.active_tab().viewport_top, 1);
    }

    #[test]
    fn ctrl_d_scrolls_half_page_down() {
        let mut a = app(100, 10);
        a.handle_key(ctrl('d'));
        assert_eq!(a.active_tab().viewport_top, 5);
    }

    #[test]
    fn space_scrolls_full_page_down() {
        let mut a = app(100, 10);
        a.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(a.active_tab().viewport_top, 10);
    }

    /// Regression: scrolling forward past the streamview's initial fetch
    /// must extend the lazy window instead of clamping at the cached
    /// records' edge.  Before the fix, `Tab::scroll_down` clamped
    /// `viewport_top` against `formatted.len()` and `maintain_window`
    /// only filled an empty window, so a user could never see beyond the
    /// initial batch (~256 records on a real engine, 100 here for speed).
    #[test]
    fn scroll_down_extends_streamview_past_initial_window() {
        use slog::{Drain, Logger, info, o};
        use std::fs::OpenOptions;
        use std::sync::Mutex;

        let dir = TestDir::new();
        let path = dir.path().join("a.log");
        {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            let drain = slog_bunyan::with_name("Nexus", file).build().fuse();
            let log = Logger::root(Mutex::new(drain).fuse(), o!());
            for i in 0..1000 {
                info!(log, "entry"; "i" => i);
            }
        }

        let mut engine = Engine::new();
        engine.add_file_source(&path).unwrap();
        let mut a = App::new_for_tests(engine);
        a.viewport_height = 10;
        let initial_len = a.active_tab().formatted().len();
        // Page down a generous number of times — each press advances by
        // viewport_height (10) lines, so 50 presses target line 500,
        // well past the bug's clamp point.
        for _ in 0..50 {
            a.handle_key(key(KeyCode::Char(' ')));
        }
        let top = a.active_tab().viewport_top.get();
        assert!(
            top > initial_len,
            "viewport_top {top} stuck at or below initial cache \
             {initial_len}; lazy window did not extend",
        );
        dir.cleanup();
    }

    #[test]
    fn ctrl_u_scrolls_half_page_up() {
        let mut a = app(100, 10);
        a.active_tab_mut().viewport_top = LineIdx(20);
        a.handle_key(ctrl('u'));
        assert_eq!(a.active_tab().viewport_top, 15);
    }

    #[test]
    fn g_jumps_top() {
        let mut a = app(100, 10);
        a.active_tab_mut().viewport_top = LineIdx(50);
        a.handle_key(key(KeyCode::Char('g')));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn home_jumps_top() {
        let mut a = app(100, 10);
        a.active_tab_mut().viewport_top = LineIdx(50);
        a.handle_key(key(KeyCode::Home));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn shift_g_jumps_bottom() {
        let mut a = app(100, 10);
        a.handle_key(shift('G'));
        assert_eq!(a.active_tab().viewport_top, 90);
    }

    #[test]
    fn end_jumps_bottom() {
        let mut a = app(100, 10);
        a.handle_key(key(KeyCode::End));
        assert_eq!(a.active_tab().viewport_top, 90);
    }

    #[test]
    fn cant_scroll_above_top() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('k')));
        assert_eq!(a.active_tab().viewport_top, 0);
        a.handle_key(ctrl('u'));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn cant_scroll_below_bottom() {
        let mut a = app(10, 5);
        a.active_tab_mut().viewport_top = LineIdx(5); // == max_top
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.active_tab().viewport_top, 5);
        a.handle_key(ctrl('d'));
        assert_eq!(a.active_tab().viewport_top, 5);
    }

    #[test]
    fn small_content_clamps_to_zero() {
        let mut a = app(3, 10);
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.active_tab().viewport_top, 0);
        a.handle_key(shift('G'));
        assert_eq!(a.active_tab().viewport_top, 0);
        a.handle_key(key(KeyCode::End));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn release_events_are_ignored() {
        let mut a = app(10, 5);
        let mut k = key(KeyCode::Char('q'));
        k.kind = KeyEventKind::Release;
        a.handle_key(k);
        assert!(!a.quit);
        assert!(a.dialog.is_none());
    }

    // ---------- top-level rendering ----------

    #[test]
    fn render_paints_rows_and_footer() {
        // Wide enough to hold the entire footer (step indicator,
        // fields toggle, ^T/^W chips) without truncation; the live
        // footer is sized to be legible at 80 cols even when terminals
        // truncate it.
        let backend = TestBackend::new(200, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec![
            "alpha line".to_string(),
            "beta line".to_string(),
            "gamma line".to_string(),
        ]);
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("alpha line"), "dump:\n{dump}");
        assert!(dump.contains("gamma line"), "dump:\n{dump}");
        // The keybindings strip is the populated stream-tab footer;
        // `h help` is a stable chip that's always present.
        assert!(dump.contains("h help"), "dump:\n{dump}");
    }

    /// When the viewport reaches the last row and no more records can
    /// arrive past it, the parse-stats line gains an "at end of
    /// stream" marker.  Test fixtures have no streamview, so reaching
    /// `bottom == total` is sufficient — same behavior as a live tab
    /// whose streamview has hit forward EOF.  Scrolling up by one
    /// line must hide the indicator again.
    #[test]
    fn render_shows_end_of_stream_when_viewport_reaches_last_row() {
        // 7 rows is enough to show all 4 logical rows plus tab bar +
        // user status + footer; the bottom of the viewport touches
        // the last row.  Keep the surface wide so the status line
        // plus the EOF suffix fits without clipping.
        let backend = TestBackend::new(300, 7);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec![
            "alpha".to_string(),
            "beta".to_string(),
            "gamma".to_string(),
            "delta".to_string(),
        ]);
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(
            dump.contains("(end of stream)"),
            "expected EOF indicator when viewport reaches last row:\n{dump}",
        );
    }

    /// Symmetric to the above: when the viewport doesn't reach the last
    /// row (the user scrolled up, or the content is taller than the
    /// viewport at first paint), the stats line omits the indicator.
    #[test]
    fn render_hides_end_of_stream_when_more_rows_below() {
        // Tiny viewport (2 content rows) over 6 logical rows: the
        // bottom of the viewport is far from the last row.
        let backend = TestBackend::new(200, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let rows: Vec<String> = (0..6).map(|i| format!("row {i}")).collect();
        let mut a = App::with_rows(rows);
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(
            !dump.contains("(end of stream)"),
            "did not expect EOF indicator with rows below:\n{dump}",
        );
    }

    /// The user-facing contract for the indicator: it appears when
    /// scrolled to the bottom and disappears as soon as the user
    /// scrolls back up.  Drives the same render path as the live TUI.
    #[test]
    fn render_eof_indicator_toggles_with_scroll() {
        let backend = TestBackend::new(300, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let rows: Vec<String> = (0..6).map(|i| format!("row {i}")).collect();
        let mut a = App::with_rows(rows);
        a.viewport_height = 2;

        // Scroll to the very bottom: indicator on.
        a.handle_key(shift('G'));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(
            dump.contains("(end of stream)"),
            "expected EOF indicator after `G`:\n{dump}",
        );

        // One `k` moves us off the bottom: indicator gone.
        a.handle_key(key(KeyCode::Char('k')));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(
            !dump.contains("(end of stream)"),
            "did not expect EOF indicator after scrolling up:\n{dump}",
        );
    }

    fn buffer_text(buf: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    // ---------- line editor (shared by dialogs) ----------

    /// Drives `LineEditor` through a sequence of typed characters,
    /// asserting each keystroke was consumed.  Mirrors `type_into` but
    /// targets the editor primitive directly so these tests stay valid
    /// regardless of which dialog embeds the editor.
    fn feed(e: &mut LineEditor, s: &str) {
        for c in s.chars() {
            assert!(matches!(
                e.handle_edit(key(KeyCode::Char(c))),
                EditAction::Handled,
            ));
        }
    }

    #[test]
    fn line_editor_typing_inserts_at_cursor() {
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "name=Nexus");
        assert_eq!(e.text, "name=Nexus");
        assert_eq!(e.cursor, "name=Nexus".len());
    }

    #[test]
    fn line_editor_backspace_deletes_char_before_cursor() {
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "abc");
        e.handle_edit(key(KeyCode::Backspace));
        assert_eq!(e.text, "ab");
        assert_eq!(e.cursor, 2);
    }

    #[test]
    fn line_editor_left_right_home_end_move_cursor() {
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "abc");
        e.handle_edit(key(KeyCode::Left));
        assert_eq!(e.cursor, 2);
        e.handle_edit(key(KeyCode::Home));
        assert_eq!(e.cursor, 0);
        e.handle_edit(key(KeyCode::Right));
        assert_eq!(e.cursor, 1);
        e.handle_edit(key(KeyCode::End));
        assert_eq!(e.cursor, 3);
    }

    #[test]
    fn line_editor_delete_removes_char_after_cursor() {
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "abc");
        e.handle_edit(key(KeyCode::Home));
        e.handle_edit(key(KeyCode::Delete));
        assert_eq!(e.text, "bc");
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn line_editor_ctrl_a_and_ctrl_e_jump_to_line_ends() {
        // ^A and ^E mirror Home and End for readline-trained users.
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "abc");
        e.handle_edit(ctrl('a'));
        assert_eq!(e.cursor, 0);
        e.handle_edit(ctrl('e'));
        assert_eq!(e.cursor, 3);
    }

    #[test]
    fn line_editor_ctrl_k_kills_to_end_of_line() {
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "level>=warn name=Nexus");
        // Position cursor at the space between predicates.
        e.handle_edit(ctrl('a'));
        for _ in 0.."level>=warn".len() {
            e.handle_edit(key(KeyCode::Right));
        }
        e.handle_edit(ctrl('k'));
        assert_eq!(e.text, "level>=warn");
        assert_eq!(e.cursor, "level>=warn".len());
    }

    #[test]
    fn line_editor_ctrl_k_at_end_is_noop() {
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "abc");
        e.handle_edit(ctrl('k'));
        assert_eq!(e.text, "abc");
        assert_eq!(e.cursor, 3);
    }

    #[test]
    fn line_editor_ctrl_u_kills_to_start_of_line() {
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "level>=warn name=Nexus");
        // Position cursor inside "Nexus".
        for _ in 0..3 {
            e.handle_edit(key(KeyCode::Left));
        }
        let cursor_before = e.cursor;
        e.handle_edit(ctrl('u'));
        assert_eq!(e.text, "xus");
        assert_eq!(e.cursor, 0);
        assert!(cursor_before > 0);
    }

    #[test]
    fn line_editor_ctrl_u_at_start_is_noop() {
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "abc");
        e.handle_edit(key(KeyCode::Home));
        e.handle_edit(ctrl('u'));
        assert_eq!(e.text, "abc");
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn line_editor_ctrl_w_kills_previous_whitespace_word() {
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "level>=warn name=Nexus");
        e.handle_edit(ctrl('w'));
        // The whole `name=Nexus` token disappears, plus the space.
        assert_eq!(e.text, "level>=warn ");
        assert_eq!(e.cursor, "level>=warn ".len());
    }

    #[test]
    fn line_editor_ctrl_w_consumes_trailing_whitespace_first() {
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "name=Nexus   ");
        e.handle_edit(ctrl('w'));
        assert_eq!(e.text, "");
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn line_editor_alt_b_moves_back_one_alphanumeric_word() {
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "level>=warn name=Nexus");
        e.handle_edit(alt('b'));
        assert_eq!(&e.text[e.cursor..], "Nexus");
        e.handle_edit(alt('b'));
        assert_eq!(&e.text[e.cursor..], "name=Nexus");
        e.handle_edit(alt('b'));
        assert_eq!(&e.text[e.cursor..], "warn name=Nexus");
        e.handle_edit(alt('b'));
        assert_eq!(e.cursor, 0);
        // Once more: clamped at zero.
        e.handle_edit(alt('b'));
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn line_editor_alt_f_moves_forward_one_alphanumeric_word() {
        let mut e = LineEditor::new(String::new());
        feed(&mut e, "level>=warn name=Nexus");
        e.handle_edit(key(KeyCode::Home));
        e.handle_edit(alt('f'));
        assert_eq!(&e.text[..e.cursor], "level");
        e.handle_edit(alt('f'));
        assert_eq!(&e.text[..e.cursor], "level>=warn");
        e.handle_edit(alt('f'));
        assert_eq!(&e.text[..e.cursor], "level>=warn name");
        e.handle_edit(alt('f'));
        assert_eq!(e.cursor, e.text.len());
        // Once more: clamped.
        e.handle_edit(alt('f'));
        assert_eq!(e.cursor, e.text.len());
    }

    // ---------- filter dialog ----------

    #[test]
    fn f_opens_filter_dialog() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        assert!(matches!(a.dialog, Some(Dialog::Filter { .. })));
    }

    #[test]
    fn dialog_keys_do_not_quit_or_scroll() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        // 'q' goes into the buffer, not to the quit binding.
        a.handle_key(key(KeyCode::Char('q')));
        assert!(!a.quit);
        assert!(a.dialog.is_some());
        // 'j' goes into the buffer, not to scroll.
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.active_tab().viewport_top, 0);
        assert_eq!(a.dialog.as_ref().unwrap().editor().unwrap().text, "qj");
    }

    #[test]
    fn dialog_prepopulates_with_current_filter() {
        let f: Filter = "level>=warn name=Nexus".parse().unwrap();
        let d = Dialog::filter(&f);
        assert_eq!(d.editor().unwrap().text, "level>=warn name=Nexus");
        // Cursor is at the end so the user can extend the filter
        // without homing first.
        assert_eq!(d.editor().unwrap().cursor, d.editor().unwrap().text.len());
        assert!(d.parse_error().is_none());
    }

    #[test]
    fn dialog_shows_parse_error_live() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "bogus");
        assert!(d.parse_error().is_some());
        let len = d.editor().unwrap().text.len();
        for _ in 0..len {
            d.handle_key(key(KeyCode::Backspace));
        }
        type_into(&mut d, "level>=warn");
        assert!(d.parse_error().is_none());
    }

    #[test]
    fn dialog_enter_with_invalid_filter_keeps_dialog_open() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "bogus");
        a.handle_key(key(KeyCode::Enter));
        let d = a.dialog.as_ref().expect("dialog should still be open");
        assert!(d.parse_error().is_some());
    }

    #[test]
    fn dialog_enter_with_valid_filter_applies_and_closes() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn");
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_none());
        assert_eq!(&a.active_stream().filter.to_string(), "level>=warn");
    }

    #[test]
    fn dialog_escape_discards_changes() {
        let mut a = app(10, 5);
        let original_filter = &a.active_stream().filter.to_string();
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "name=Nexus");
        a.handle_key(key(KeyCode::Esc));
        assert!(a.dialog.is_none());
        assert_eq!(&a.active_stream().filter.to_string(), original_filter);
    }

    #[test]
    fn dialog_apply_resets_viewport_and_requeries_engine() {
        // Build a real engine with a tiny bunyan file so apply_filter
        // can re-run query_events.  This is the only filter-dialog
        // test that isn't pure state-machine.
        use slog::{Drain, Logger, info, o, warn};
        use std::fs::OpenOptions;
        use std::sync::Mutex;

        let dir = TestDir::new();
        let path = dir.path().join("a.log");
        {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            let drain = slog_bunyan::with_name("Nexus", file).build().fuse();
            let log = Logger::root(Mutex::new(drain).fuse(), o!());
            for _ in 0..5 {
                info!(log, "info entry");
            }
            warn!(log, "warn entry");
        }

        let mut engine = Engine::new();
        engine.add_file_source(&path).unwrap();
        let mut a = App::new_for_tests(engine);
        a.viewport_height = 2;
        a.active_tab_mut().viewport_top = LineIdx(3);
        assert_eq!(a.active_tab().formatted().len(), 6);

        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn");
        a.handle_key(key(KeyCode::Enter));
        a.drain_long_op();

        assert!(a.dialog.is_none());
        assert_eq!(a.active_tab().formatted().len(), 1);
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn render_draws_dialog_with_error() {
        let backend = TestBackend::new(80, 9);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "bogus");
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("Filter"), "dump:\n{dump}");
        assert!(dump.contains("bogus"), "dump:\n{dump}");
        assert!(
            dump.contains("operator") || dump.contains("token"),
            "expected a parse error in dump:\n{dump}",
        );
    }

    #[test]
    fn filter_dialog_wraps_long_text() {
        // With a narrow terminal a long filter string should wrap onto
        // additional rows inside the popup rather than being clipped at
        // the right edge.  Popup width is 70% of 40 = 28 (inner 26), so
        // a 60-char filter wraps onto 3 rows.
        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('f')));
        let filter = "level>=warn name=Nexus name=sled-agent name=cockroach";
        type_into(a.dialog.as_mut().unwrap(), filter);
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        // The bug we're guarding against is the tail being clipped
        // past the popup's right edge — every chunk of the filter must
        // appear somewhere in the rendered buffer.  Substrings are
        // chosen to fit within a single 26-column wrapped row so the
        // assertion isn't broken by intra-substring wrap points.
        assert!(dump.contains("level>=warn"), "missing prefix in:\n{dump}");
        assert!(dump.contains("sled-agent"), "missing middle in:\n{dump}");
        assert!(dump.contains("cockroac"), "missing tail in:\n{dump}");
        // The popup grew vertically to accommodate the wrap: count
        // rows containing a left-border glyph.  3 wrapped editor rows
        // + 2 error-padding rows = 5 interior rows.
        let interior_rows = dump.lines().filter(|l| l.contains('│')).count();
        assert_eq!(interior_rows, 5, "expected 5 interior rows in:\n{dump}");
    }

    #[test]
    fn wrap_dialog_text_handles_empty_and_chunking() {
        // Empty input: caller still needs a row for the cursor.
        let lines = wrap_dialog_text("", 10);
        assert_eq!(lines.len(), 1);

        // Exact multiple of width yields the minimum number of rows;
        // the cursor-at-end fixup in render_dialog handles placement.
        let lines = wrap_dialog_text("abcdef", 3);
        assert_eq!(lines.len(), 2);

        // Non-multiple wraps to ceil(len/width) rows.
        let lines = wrap_dialog_text("abcdefg", 3);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn render_draws_display_fields_dialog() {
        // Open the dialog and confirm the rendered popup carries the
        // title, every item label, and the right glyphs for the
        // default state (timestamp = date+ms, hostname = short, name
        // checked, pid + extras unchecked).
        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('d')));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("Display fields"), "dump:\n{dump}");
        for label in [
            "short timestamp (no date)",
            "full date and time",
            "short hostname",
            "full hostname",
            "no hostname",
            "pid",
            "name",
            "show all other fields",
        ] {
            assert!(dump.contains(label), "missing {label:?} in:\n{dump}");
        }
        // Default radios: timestamp long is on, hostname short is on.
        assert!(
            dump.contains("(•) full date and time"),
            "expected full-date radio selected:\n{dump}",
        );
        assert!(
            dump.contains("(•) short hostname"),
            "expected short hostname radio selected:\n{dump}",
        );
        // pid and extras default off; name defaults on.
        assert!(dump.contains("[ ] pid"), "expected pid unchecked:\n{dump}");
        assert!(dump.contains("[x] name"), "expected name checked:\n{dump}",);
        assert!(
            dump.contains("[ ] show all other fields"),
            "expected extras unchecked:\n{dump}",
        );
    }

    // ---------- help popup ----------

    #[test]
    fn h_opens_help_dialog() {
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('h')));
        assert!(
            matches!(a.dialog, Some(Dialog::Help { scroll: 0 })),
            "expected Help dialog, got {:?}",
            a.dialog.as_ref().map(|_| "<other>"),
        );
    }

    #[test]
    fn help_dialog_esc_closes() {
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('h')));
        a.handle_key(key(KeyCode::Esc));
        assert!(a.dialog.is_none());
    }

    #[test]
    fn help_dialog_enter_closes() {
        // Enter is bound to "close" (same as Esc) on the read-only help
        // popup; matches the SeeitCommand popup's behavior.
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('h')));
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_none());
    }

    #[test]
    fn help_dialog_j_and_k_scroll() {
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('h')));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('j')));
        match a.dialog {
            Some(Dialog::Help { scroll }) => assert_eq!(scroll, 2),
            _ => panic!("expected Help dialog after j×2"),
        }
        a.handle_key(key(KeyCode::Char('k')));
        match a.dialog {
            Some(Dialog::Help { scroll }) => assert_eq!(scroll, 1),
            _ => panic!("expected Help dialog after k"),
        }
        // k past 0 saturates rather than wrapping; otherwise scrolling
        // back up from line 0 would jump to the end of the body, which
        // would be jarring.
        a.handle_key(key(KeyCode::Char('k')));
        a.handle_key(key(KeyCode::Char('k')));
        match a.dialog {
            Some(Dialog::Help { scroll }) => assert_eq!(scroll, 0),
            _ => panic!("expected Help dialog after k saturate"),
        }
    }

    #[test]
    fn help_dialog_does_not_quit_on_q() {
        // While the help popup is open, `q` is dropped (not routed to
        // the underlying app's quit prompt).  Same protection the other
        // editor-less dialogs have, so a stray keystroke can't tear
        // down the user's session by accident.
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('h')));
        a.handle_key(key(KeyCode::Char('q')));
        assert!(matches!(a.dialog, Some(Dialog::Help { .. })));
    }

    #[test]
    fn help_sections_contain_expected_bindings() {
        // Anchor the section taxonomy and the few bindings most likely
        // to drift (`h` for help itself, `d` for the display dialog —
        // historically lived on `h`, easy to revert by accident).
        let titles: Vec<&str> = HELP_SECTIONS.iter().map(|s| s.title).collect();
        assert!(titles.contains(&"Navigation"), "got {titles:?}");
        assert!(titles.contains(&"Display"), "got {titles:?}");
        assert!(titles.contains(&"Other"), "got {titles:?}");
        assert!(titles.contains(&"Bookmarks pane"), "got {titles:?}");

        let all: Vec<(&str, &str)> = HELP_SECTIONS
            .iter()
            .flat_map(|s| s.items.iter().copied())
            .collect();
        assert!(
            all.iter().any(|(k, _)| *k == "h"),
            "expected `h` binding in help, got {all:?}",
        );
        assert!(
            all.iter().any(|(k, v)| *k == "d" && v.contains("display")),
            "expected `d` → display in help, got {all:?}",
        );
    }

    #[test]
    fn render_draws_help_dialog() {
        // Opening the help popup and rendering one frame should put
        // the dialog title and the first section's heading and items
        // on screen.  Use a tall backend so the body isn't truncated
        // by the popup height clamp.
        let backend = TestBackend::new(120, 60);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('h')));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("Keybindings"), "missing title:\n{dump}");
        for section in ["Navigation", "Display", "Other", "Bookmarks pane"] {
            assert!(dump.contains(section), "missing {section}:\n{dump}");
        }
        // A representative binding from each of those sections — anchors
        // the description column so a change to `build_help_lines`
        // formatting would be caught here.
        for desc in [
            "scroll down one line",  // Navigation
            "display-fields dialog", // Display
            "show this help",        // Other
            "delete bookmark",       // Bookmarks pane
        ] {
            assert!(
                dump.contains(desc),
                "missing description {desc:?}:\n{dump}"
            );
        }
    }

    // ---------- tabs ----------

    #[test]
    fn fresh_app_has_one_tab_named_tab_one() {
        let a = app(0, 5);
        assert_eq!(a.tabs.len(), 1);
        assert_eq!(a.active, 0);
        assert_eq!(a.active_tab().name, "Tab 1");
    }

    #[test]
    fn ctrl_t_creates_new_tab_with_cloned_filter_and_opens_dialog() {
        let mut a = app(10, 5);
        // Set a filter on the first tab.
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.tabs.len(), 1);

        a.handle_key(ctrl('t'));
        assert_eq!(a.tabs.len(), 2);
        assert_eq!(a.active, 1);
        assert_eq!(&a.active_stream().filter.to_string(), "level>=warn");
        // Filter dialog is open with the cloned filter prefilled.
        let d = a.dialog.as_ref().expect("dialog should be open");
        assert!(matches!(d, Dialog::Filter { .. }));
        assert_eq!(d.editor().unwrap().text, "level>=warn");
    }

    #[test]
    fn ctrl_t_uses_monotonic_tab_numbering() {
        let mut a = app(10, 5);
        assert_eq!(a.active_tab().name, "Tab 1");
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.active_tab().name, "Tab 2");
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.active_tab().name, "Tab 3");
    }

    #[test]
    fn esc_on_new_tab_dialog_keeps_tab_with_cloned_filter() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn");
        a.handle_key(key(KeyCode::Enter));

        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.tabs.len(), 2);
        assert_eq!(a.active, 1);
        assert_eq!(&a.active_stream().filter.to_string(), "level>=warn");
    }

    #[test]
    fn tab_cycles_to_next_with_wrap() {
        let mut a = app(10, 5);
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.tabs.len(), 3);
        assert_eq!(a.active, 2);
        a.handle_key(key(KeyCode::Tab));
        assert_eq!(a.active, 0);
        a.handle_key(key(KeyCode::Tab));
        assert_eq!(a.active, 1);
    }

    #[test]
    fn shift_tab_cycles_to_previous_with_wrap() {
        let mut a = app(10, 5);
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.tabs.len(), 2);
        assert_eq!(a.active, 1);
        a.handle_key(back_tab());
        assert_eq!(a.active, 0);
        // Wrap.
        a.handle_key(back_tab());
        assert_eq!(a.active, 1);
    }

    #[test]
    fn ctrl_w_closes_active_tab() {
        let mut a = app(10, 5);
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.tabs.len(), 2);
        let surviving_name = a.tabs[0].name.clone();
        a.handle_key(ctrl('w'));
        assert_eq!(a.tabs.len(), 1);
        assert_eq!(a.tabs[0].name, surviving_name);
    }

    #[test]
    fn ctrl_w_on_last_tab_creates_fresh_one() {
        let mut a = app(10, 5);
        assert_eq!(a.tabs.len(), 1);
        let original_name = a.active_tab().name.clone();
        a.handle_key(ctrl('w'));
        assert_eq!(a.tabs.len(), 1);
        assert_eq!(a.active, 0);
        // It's a new tab — different name (next number).
        assert_ne!(a.active_tab().name, original_name);
        assert!(&a.active_stream().filter.predicates().is_empty());
    }

    #[test]
    fn each_tab_keeps_its_own_viewport_top() {
        let mut a = app(100, 10);
        a.active_tab_mut().viewport_top = LineIdx(30);
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.active_tab().viewport_top, 0);
        a.active_tab_mut().viewport_top = LineIdx(50);
        a.handle_key(back_tab());
        assert_eq!(a.active_tab().viewport_top, 30);
        a.handle_key(key(KeyCode::Tab));
        assert_eq!(a.active_tab().viewport_top, 50);
    }

    #[test]
    fn ctrl_w_inside_filter_dialog_kills_word_not_close_tab() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn name=Nexus");
        a.handle_key(ctrl('w'));
        // Dialog still open, buffer lost its last word.
        assert!(a.dialog.is_some());
        assert_eq!(
            a.dialog.as_ref().unwrap().editor().unwrap().text,
            "level>=warn "
        );
        // Tab count unchanged.
        assert_eq!(a.tabs.len(), 1);
    }

    // ---------- rename dialog ----------

    #[test]
    fn r_opens_rename_dialog_with_current_name() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('r')));
        let d = a.dialog.as_ref().expect("dialog should be open");
        assert!(matches!(d, Dialog::Rename { .. }));
        assert_eq!(d.editor().unwrap().text, "Tab 1");
    }

    #[test]
    fn rename_dialog_enter_applies_new_name() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('r')));
        // Clear and type a new name.
        a.handle_key(ctrl('u'));
        type_into(a.dialog.as_mut().unwrap(), "Nexus");
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_none());
        assert_eq!(a.active_tab().name, "Nexus");
    }

    #[test]
    fn rename_dialog_escape_discards() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('r')));
        type_into(a.dialog.as_mut().unwrap(), "garbage");
        a.handle_key(key(KeyCode::Esc));
        assert!(a.dialog.is_none());
        assert_eq!(a.active_tab().name, "Tab 1");
    }

    #[test]
    fn rename_dialog_has_no_parse_error() {
        // Anything (including invalid filter syntax) is acceptable for
        // a tab name — the rename dialog should never report a parse
        // error.
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('r')));
        type_into(a.dialog.as_mut().unwrap(), " not a filter ");
        assert!(a.dialog.as_ref().unwrap().parse_error().is_none());
    }

    // ---------- main pane wrap ----------

    #[test]
    fn main_pane_wraps_long_logical_lines() {
        // A logical line longer than the terminal width should appear
        // on multiple visual rows in the rendered buffer.  Using
        // ASCII-only content keeps the char-count math straightforward.
        let long = "A".repeat(80);
        let mut a = App::with_rows(vec![long.clone()]);
        // 4 tabs rows total: tab bar (1) + content (n) + stats (1) +
        // footer (1).  With 5 rows of viewport and 20-col width, an
        // 80-char line wraps to 4 visual rows — all should fit.
        let backend = TestBackend::new(20, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        // 80 'A's at 20-col width = 4 rows of 'A's.  Count buffer rows
        // whose content is entirely 'A's (the content area starts
        // below the tab bar).
        let full_a_rows =
            dump.lines().filter(|l| l.chars().all(|c| c == 'A')).count();
        assert_eq!(
            full_a_rows, 4,
            "expected 4 wrapped rows of A's, dump:\n{dump}",
        );
    }

    #[test]
    fn raw_mode_wraps_at_column_boundary_not_whitespace() {
        // The raw JSON written by the bunyan logger contains spaces
        // between key/value pairs.  Word-wrap would break at one of
        // those spaces; raw mode wraps at the column boundary
        // instead, so a copy of the wrapped rows can be re-joined by
        // stripping newlines.  Verify by rendering a real raw line
        // at a narrow viewport and asserting that the visual rows
        // are exactly the next N chars of the source, including
        // spaces that would otherwise have been the wrap point.
        let (mut a, dir) = multi_line_app(&[(10, "first message", &[])]);
        a.toggle_show_raw();
        // Wide enough to fit the JSON in a few rows of column wrap;
        // narrow enough that any whitespace inside it would be a
        // word-wrap candidate.
        let backend = TestBackend::new(30, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());

        // The active stream's first formatted line is the raw JSON
        // for the single event.  Compute the expected chunks and
        // confirm each appears in the dump.
        let raw = a.active_tab().formatted()[0].clone();
        assert!(
            raw.starts_with('{') && raw.contains(r#""msg":"first message""#),
            "raw line should be JSON: {raw}",
        );
        for chunk in column_chunks(&raw, 30) {
            assert!(
                dump.contains(chunk),
                "expected column chunk {chunk:?} in dump:\n{dump}",
            );
        }
        dir.cleanup();
    }

    #[test]
    fn column_chunks_handles_empty_and_unicode() {
        assert_eq!(column_chunks("", 4), vec![""]);
        assert_eq!(column_chunks("abcdefghij", 4), vec!["abcd", "efgh", "ij"],);
        // Greek letters are 2 bytes each in UTF-8.  Char-based
        // counting must produce 4-char chunks regardless.
        assert_eq!(column_chunks("αβγδεζηθικ", 4), vec!["αβγδ", "εζηθ", "ικ"],);
        // Width 0 is degenerate; return the whole line so we don't
        // loop forever or divide by zero.
        assert_eq!(column_chunks("abc", 0), vec!["abc"]);
    }

    #[test]
    fn max_top_accounts_for_wrap() {
        // 5 short lines and 1 long line that wraps to 4 visual rows.
        // viewport_height = 10 (visual), viewport_width = 10.  Total
        // visual rows: 5 + 4 = 9, so max_top should be 0 (everything
        // fits).
        let mut rows: Vec<String> = (0..5).map(|i| format!("r{i}")).collect();
        rows.push("X".repeat(40)); // wraps to ceil(40/10) = 4 rows
        let mut a = App::with_rows(rows);
        a.viewport_height = 10;
        a.viewport_width = 10;
        let max = a.active_tab().max_top(a.viewport_height, a.viewport_width);
        assert_eq!(max, 0, "buffer fits, max_top should be 0");

        // Shrink viewport so the long line barely fits but the shorts
        // don't all join.  viewport_height = 5: only the wrapped line
        // (4 rows) and one of the short lines (1 row) fit.  max_top
        // should be 4 (start from row index 4 = the last short row).
        a.viewport_height = 5;
        let max = a.active_tab().max_top(a.viewport_height, a.viewport_width);
        assert_eq!(max, 4, "should leave room for short row + wrap");
    }

    #[test]
    fn max_top_caps_for_oversized_last_line() {
        // A single line whose wrap exceeds the viewport on its own
        // should still scroll to itself (not past it), so the user
        // can see its start even though its tail is clipped.
        let big = "Z".repeat(200);
        let mut a = App::with_rows(vec!["short".to_string(), big]);
        a.viewport_height = 5;
        a.viewport_width = 20; // big wraps to 10 rows
        let max = a.active_tab().max_top(a.viewport_height, a.viewport_width);
        // Last index is 1; max_top is capped to that so the giant
        // line is reachable as the top of the viewport.
        assert_eq!(max, 1);
    }

    // ---------- tab-bar rendering ----------

    #[test]
    fn render_paints_tab_bar() {
        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("Tab 1"), "dump:\n{dump}");
        assert!(dump.contains("Tab 2"), "dump:\n{dump}");
    }

    // ---------- search ----------

    /// 10 rows whose every-other line matches "alpha" — useful for
    /// exercising forward and backward navigation across multiple
    /// matches.
    fn search_app() -> App {
        let rows = (0..10)
            .map(|i| {
                if i % 2 == 0 {
                    format!("alpha row {i}")
                } else {
                    format!("beta row {i}")
                }
            })
            .collect();
        let mut a = App::with_rows(rows);
        a.viewport_height = 3;
        a
    }

    #[test]
    fn slash_opens_forward_search_dialog() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        let d = a.dialog.as_ref().expect("search dialog should be open");
        assert!(matches!(
            d,
            Dialog::Search { direction: SearchDirection::Forward, .. }
        ));
        assert_eq!(d.editor().unwrap().text, "");
    }

    #[test]
    fn question_opens_backward_search_dialog() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('?')));
        let d = a.dialog.as_ref().expect("search dialog should be open");
        assert!(matches!(
            d,
            Dialog::Search { direction: SearchDirection::Backward, .. }
        ));
    }

    #[test]
    fn search_dialog_keys_do_not_quit_or_scroll() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        // 'q' goes into the buffer, not to the quit binding.
        a.handle_key(key(KeyCode::Char('q')));
        assert!(!a.quit);
        assert!(a.dialog.is_some());
        // 'j' goes into the buffer, not to scroll.
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.active_tab().viewport_top, 0);
        assert_eq!(a.dialog.as_ref().unwrap().editor().unwrap().text, "qj");
    }

    #[test]
    fn ctrl_w_inside_search_dialog_kills_word_not_close_tab() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha beta");
        a.handle_key(ctrl('w'));
        assert!(a.dialog.is_some());
        assert_eq!(a.dialog.as_ref().unwrap().editor().unwrap().text, "alpha ");
        assert_eq!(a.tabs.len(), 1);
    }

    #[test]
    fn backspace_on_empty_search_prompt_cancels_dialog() {
        // Open the prompt, type one character, then backspace twice:
        // the first delete returns the buffer to empty, the second is
        // the "back over the leading `/`" gesture and should close the
        // dialog without applying anything.
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        a.handle_key(key(KeyCode::Char('a')));
        a.handle_key(key(KeyCode::Backspace));
        assert!(a.dialog.is_some());
        assert_eq!(a.dialog.as_ref().unwrap().editor().unwrap().text, "");
        a.handle_key(key(KeyCode::Backspace));
        assert!(a.dialog.is_none());
        assert!(a.last_search.is_none());
        assert!(a.active_tab().search.is_none());
    }

    #[test]
    fn backspace_on_freshly_opened_search_prompt_cancels_dialog() {
        // No characters typed at all: the very first Backspace should
        // close the dialog, mirroring less / vim's behaviour.
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        assert!(a.dialog.is_some());
        a.handle_key(key(KeyCode::Backspace));
        assert!(a.dialog.is_none());
    }

    #[test]
    fn backspace_on_empty_filter_dialog_does_not_cancel() {
        // The empty-buffer-Backspace shortcut is search-only; the
        // filter dialog should keep its (preexisting) behaviour of
        // treating Backspace as a no-op when there's nothing to delete.
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('f')));
        assert!(a.dialog.is_some());
        // Clear whatever default text the filter dialog opened with.
        let len = a.dialog.as_ref().unwrap().editor().unwrap().text.len();
        for _ in 0..len {
            a.handle_key(key(KeyCode::Backspace));
        }
        assert_eq!(a.dialog.as_ref().unwrap().editor().unwrap().text, "");
        a.handle_key(key(KeyCode::Backspace));
        assert!(a.dialog.is_some(), "filter dialog should stay open");
    }

    /// Returns the current search-dialog editor buffer, panicking
    /// (with a clear message) if the dialog isn't open or isn't a
    /// search dialog.  Keeps the history navigation tests compact.
    fn search_buffer(a: &App) -> &str {
        match a.dialog.as_ref().expect("search dialog should be open") {
            Dialog::Search { editor, .. } => editor.text.as_str(),
            _ => panic!("expected Dialog::Search"),
        }
    }

    #[test]
    fn applying_search_records_pattern_in_session_history() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.session.search_history, vec!["alpha"]);

        // A second, distinct search lands at the front.
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "beta");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.session.search_history, vec!["beta", "alpha"]);

        // Re-running "alpha" moves it back to the front rather than
        // duplicating it.
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.session.search_history, vec!["alpha", "beta"]);
    }

    #[test]
    fn search_dialog_up_walks_history_oldest_direction() {
        let mut a = search_app();
        a.session.search_history =
            vec!["gamma".into(), "beta".into(), "alpha".into()];
        a.handle_key(key(KeyCode::Char('/')));
        // No prefix typed: Up walks every entry in MRU order.
        a.handle_key(key(KeyCode::Up));
        assert_eq!(search_buffer(&a), "gamma");
        a.handle_key(key(KeyCode::Up));
        assert_eq!(search_buffer(&a), "beta");
        a.handle_key(key(KeyCode::Up));
        assert_eq!(search_buffer(&a), "alpha");
        // Past the end: stays on the oldest entry.
        a.handle_key(key(KeyCode::Up));
        assert_eq!(search_buffer(&a), "alpha");
    }

    #[test]
    fn search_dialog_down_restores_originally_typed_text() {
        let mut a = search_app();
        a.session.search_history = vec!["gamma".into(), "beta".into()];
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "ne");
        // No history entry starts with "ne", so Up should be a no-op
        // and Down with nothing to walk back to is also a no-op.
        a.handle_key(key(KeyCode::Up));
        assert_eq!(search_buffer(&a), "ne");
        a.handle_key(key(KeyCode::Down));
        assert_eq!(search_buffer(&a), "ne");
    }

    #[test]
    fn search_dialog_up_filters_history_by_typed_prefix() {
        let mut a = search_app();
        a.session.search_history = vec![
            "nexus".into(),
            "beta".into(),
            "name=nexus".into(),
            "needle".into(),
        ];
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "ne");
        // Up walks only entries that start with "ne".
        a.handle_key(key(KeyCode::Up));
        assert_eq!(search_buffer(&a), "nexus");
        a.handle_key(key(KeyCode::Up));
        assert_eq!(search_buffer(&a), "needle");
        // Past the end: stays on "needle".
        a.handle_key(key(KeyCode::Up));
        assert_eq!(search_buffer(&a), "needle");
        // Down walks back through the same matches.
        a.handle_key(key(KeyCode::Down));
        assert_eq!(search_buffer(&a), "nexus");
        // Down past the front: the editor returns to "ne", the
        // prefix the user originally typed.
        a.handle_key(key(KeyCode::Down));
        assert_eq!(search_buffer(&a), "ne");
        // And subsequent Down does nothing.
        a.handle_key(key(KeyCode::Down));
        assert_eq!(search_buffer(&a), "ne");
    }

    #[test]
    fn editing_after_history_walk_uses_new_buffer_as_prefix() {
        // Bring "alpha" into the editor via history, then append `1`.
        // The next Up press should look for entries starting with
        // "alpha1" (none here), not continue walking "alpha"-prefixed
        // history.
        let mut a = search_app();
        a.session.search_history =
            vec!["alpha2".into(), "alpha".into(), "beta".into()];
        a.handle_key(key(KeyCode::Char('/')));
        a.handle_key(key(KeyCode::Up));
        assert_eq!(search_buffer(&a), "alpha2");
        a.handle_key(key(KeyCode::Up));
        assert_eq!(search_buffer(&a), "alpha");
        a.handle_key(key(KeyCode::Char('1')));
        assert_eq!(search_buffer(&a), "alpha1");
        // No history entry begins with "alpha1"; Up is a no-op.
        a.handle_key(key(KeyCode::Up));
        assert_eq!(search_buffer(&a), "alpha1");
    }

    #[test]
    fn search_dialog_shows_parse_error_live() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        // `[` opens an unclosed character class.
        type_into(a.dialog.as_mut().unwrap(), "[");
        assert!(a.dialog.as_ref().unwrap().parse_error().is_some());
        a.handle_key(key(KeyCode::Backspace));
        assert!(a.dialog.as_ref().unwrap().parse_error().is_none());
    }

    #[test]
    fn search_enter_with_invalid_regex_keeps_dialog_open() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "[");
        a.handle_key(key(KeyCode::Enter));
        let d = a.dialog.as_ref().expect("dialog should still be open");
        assert!(d.parse_error().is_some());
        assert!(a.last_search.is_none());
        assert!(a.active_tab().search.is_none());
    }

    #[test]
    fn search_enter_with_valid_regex_jumps_to_first_match() {
        let mut a = search_app();
        // Start mid-file so "first match at or after viewport_top" is
        // visible.
        a.active_tab_mut().viewport_top = LineIdx(3);
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        a.handle_key(key(KeyCode::Enter));

        assert!(a.dialog.is_none());
        // Rows 4, 6, 8 are alphas (and 0, 2 above); the first match
        // at or after row 3 is row 4.
        assert_eq!(a.active_tab().viewport_top, 4);
        assert_eq!(
            a.last_search.as_ref().unwrap().pattern,
            "alpha".to_string(),
        );
        assert_eq!(
            a.last_search.as_ref().unwrap().direction,
            SearchDirection::Forward,
        );
        // tab.search holds all five even-row indices.
        let s = a.active_tab().search.as_ref().unwrap();
        assert_eq!(s.matches, vec![0, 2, 4, 6, 8]);
    }

    #[test]
    fn backward_search_jumps_to_previous_match() {
        let mut a = search_app();
        a.active_tab_mut().viewport_top = LineIdx(5);
        a.handle_key(key(KeyCode::Char('?')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        a.handle_key(key(KeyCode::Enter));

        // Closest alpha at or before row 5 is row 4.
        assert_eq!(a.active_tab().viewport_top, 4);
        assert_eq!(
            a.last_search.as_ref().unwrap().direction,
            SearchDirection::Backward,
        );
    }

    #[test]
    fn n_advances_to_next_match() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        a.handle_key(key(KeyCode::Enter));
        // Lands on row 0 (first match at/after viewport_top=0).
        assert_eq!(a.active_tab().viewport_top, 0);
        a.handle_key(key(KeyCode::Char('n')));
        assert_eq!(a.active_tab().viewport_top, 2);
        a.handle_key(key(KeyCode::Char('n')));
        assert_eq!(a.active_tab().viewport_top, 4);
    }

    #[test]
    fn shift_n_reverses_direction_for_one_step_only() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        a.handle_key(key(KeyCode::Enter));
        a.handle_key(key(KeyCode::Char('n')));
        a.handle_key(key(KeyCode::Char('n')));
        // viewport_top now at row 4.
        assert_eq!(a.active_tab().viewport_top, 4);
        a.handle_key(shift('N'));
        assert_eq!(a.active_tab().viewport_top, 2);
        // last_search direction is still Forward, so `n` resumes
        // going forward (back to row 4).
        a.handle_key(key(KeyCode::Char('n')));
        assert_eq!(a.active_tab().viewport_top, 4);
    }

    #[test]
    fn shift_g_installs_seek_long_op() {
        // `G` should hand the window-fill off to a [`LongOp::Seek`]
        // so the UI stays responsive when a selective filter has to
        // walk many on-disk records to find the last viewport_height
        // matching ones.  Without the long-op the keypress would
        // block the event loop until the synchronous walk completed.
        let msgs: Vec<String> =
            (0..50).map(|i| format!("payload-m{i}")).collect();
        let records: Vec<(i64, &str)> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| (10 + i as i64, m.as_str()))
            .collect();
        let (mut a, dir) = host_app("test", &records);
        a.handle_key(shift('G'));
        assert!(
            matches!(a.long_op, Some(LongOp::Seek(_))),
            "G should install a Seek long op, got {:?}",
            a.long_op.as_ref().map(|o| o.label()),
        );
        if let Some(LongOp::Seek(s)) = a.long_op.as_ref() {
            assert!(matches!(s.finalize, SeekFinalize::Back));
            assert_eq!(s.label, "Seeking to end");
        }
        a.drain_long_op();
        // After draining the op the streamview is fully populated and
        // the viewport is at EOF (max_top clamped).
        assert!(a.long_op.is_none());
        assert!(a.active_tab().viewport_top > 0);
        dir.cleanup();
    }

    #[test]
    fn g_installs_seek_long_op() {
        let msgs: Vec<String> =
            (0..50).map(|i| format!("payload-m{i}")).collect();
        let records: Vec<(i64, &str)> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| (10 + i as i64, m.as_str()))
            .collect();
        let (mut a, dir) = host_app("test", &records);
        // First scroll somewhere away from the top so `g` has to seek.
        a.handle_key(shift('G'));
        a.drain_long_op();
        assert!(a.active_tab().viewport_top > 0);
        a.handle_key(key(KeyCode::Char('g')));
        assert!(
            matches!(a.long_op, Some(LongOp::Seek(_))),
            "g should install a Seek long op, got {:?}",
            a.long_op.as_ref().map(|o| o.label()),
        );
        if let Some(LongOp::Seek(s)) = a.long_op.as_ref() {
            assert!(matches!(s.finalize, SeekFinalize::Front));
            assert_eq!(s.label, "Seeking to start");
        }
        a.drain_long_op();
        assert_eq!(a.active_tab().viewport_top, 0);
        dir.cleanup();
    }

    #[test]
    fn apply_filter_installs_seek_long_op_for_active_tab() {
        // Filter changes on the active tab also defer the streamview
        // rebuild to a long op so the user sees a progress bar
        // instead of a freeze on selective filters.
        let msgs: Vec<String> =
            (0..50).map(|i| format!("payload-m{i}")).collect();
        let records: Vec<(i64, &str)> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| (10 + i as i64, m.as_str()))
            .collect();
        let (mut a, dir) = host_app("test", &records);
        let filter: Filter = "msg=payload-m25".parse().unwrap();
        a.apply_filter(filter);
        assert!(
            matches!(a.long_op, Some(LongOp::Seek(_))),
            "apply_filter should install a Seek long op",
        );
        if let Some(LongOp::Seek(s)) = a.long_op.as_ref() {
            assert_eq!(s.label, "Applying filter");
        }
        a.drain_long_op();
        // After the op, the only matching record is visible.
        let formatted = a.active_tab().formatted();
        assert!(
            formatted.iter().any(|l| l.contains("payload-m25")),
            "expected 'payload-m25' in viewport, got {formatted:?}",
        );
        dir.cleanup();
    }

    #[test]
    fn seek_long_op_does_not_waste_work_under_selective_filter() {
        // Regression check for a subtle bug: when `ensure_window_step`
        // used the default per-fill batch size (64) but only consumed
        // one matching record per tick, every tick threw away the
        // remaining 63 buffered matches, multiplying the walk count
        // ~64x.  The fix uses a small per-fill batch size so each
        // tick walks just enough to surface the matches it actually
        // returns.  Verify the parse-stats records grow linearly,
        // not in giant jumps.
        let msgs: Vec<String> =
            (0..500).map(|i| format!("payload-m{i}")).collect();
        let records: Vec<(i64, &str)> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| (10 + i as i64, m.as_str()))
            .collect();
        let (mut a, dir) = host_app("test", &records);
        let filter: Filter = "msg=~payload-m[0-9]*99$".parse().unwrap();
        a.apply_filter(filter);
        a.drain_long_op();
        // Drained: tab should hold the 5 matching records (m99, m199,
        // m299, m399, m499).  parse_stats.records counts records
        // appended to the streamview — without the small-batch fix
        // each tick would have buffered (and discarded) up to 64
        // matches, but our file has only 5 anyway so the test is
        // about end-state correctness here: every match landed, and
        // nothing got double-counted from re-walking.
        let view_records = a.active_tab().streamview.as_ref().unwrap();
        assert_eq!(view_records.materialized().events.len(), 5);
        dir.cleanup();
    }

    #[test]
    fn seek_long_op_yields_between_ticks_under_selective_filter() {
        // Build a many-record file and apply a filter selective
        // enough that fetching the full target_lines would take many
        // matches (and many on-disk walks per match).  The long-op
        // must yield between ticks rather than walking the whole
        // file in one synchronous call — verified here by observing
        // that the first `advance_long_op` call returns `NotDone`
        // while the SeekOp is mid-flight (records cached < target).
        let msgs: Vec<String> =
            (0..2000).map(|i| format!("payload-m{i}")).collect();
        let records: Vec<(i64, &str)> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| (10 + i as i64, m.as_str()))
            .collect();
        let (mut a, dir) = host_app("test", &records);
        // Selective filter: only multiples of 100 (20 records out of
        // 2000) survive.  Without per-tick yielding, finding any one
        // match still has to walk an average of ~100 records on
        // disk, and finding viewport_height + over-fetch matches
        // would block the UI for the entire op.
        let filter: Filter = "msg=~payload-m[0-9]*00$".parse().unwrap();
        a.apply_filter(filter);
        // Apply_filter installed the SeekOp; the very first
        // advance_long_op tick should *not* finish the op (it yields
        // after one frame's worth of work).
        let done = a.advance_long_op();
        assert!(!done, "first long-op tick should yield rather than finish",);
        assert!(matches!(a.long_op, Some(LongOp::Seek(_))));
        // Drain the rest and confirm the op eventually completes.
        a.drain_long_op();
        assert!(a.long_op.is_none());
        dir.cleanup();
    }

    #[test]
    fn cancel_seek_leaves_partial_window() {
        // Ctrl-C mid-seek must leave a usable view: the partial
        // records fetched so far should be visible, with a notice
        // telling the user the op stopped early.  We can simulate
        // this by installing a seek and cancelling before drain.
        let msgs: Vec<String> =
            (0..50).map(|i| format!("payload-m{i}")).collect();
        let records: Vec<(i64, &str)> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| (10 + i as i64, m.as_str()))
            .collect();
        let (mut a, dir) = host_app("test", &records);
        a.handle_key(shift('G'));
        assert!(matches!(a.long_op, Some(LongOp::Seek(_))));
        a.cancel_long_op();
        assert!(a.long_op.is_none());
        assert!(
            a.notice.as_deref().is_some_and(|n| n.contains("cancelled")),
            "expected a cancellation notice, got {:?}",
            a.notice,
        );
        dir.cleanup();
    }

    #[test]
    fn k_after_seek_to_end_advances_viewport_top() {
        // User-reported regression: `G` to scroll to EOF, then `k`
        // appeared to do nothing.  Root cause: `seek_to_end` sets the
        // streamview's anchor to the last line, the TUI's `resync`
        // clamps `viewport_top` to `max_top`, and the anchor and
        // `viewport_top` drift apart.  Subsequent `k` keystrokes move
        // the anchor backward but the clamp pins `viewport_top` to
        // `max_top` until the anchor crosses below it (which takes
        // ~viewport_height presses).  Fix: when the anchor lands past
        // `max_top`, sync it back so the next `k` moves the visible
        // viewport on the first keystroke.
        let msgs: Vec<String> =
            (0..300).map(|i| format!("payload-m{i}")).collect();
        let records: Vec<(i64, &str)> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| (10 + i as i64, m.as_str()))
            .collect();
        let (mut a, dir) = host_app("test", &records);
        a.handle_key(shift('G'));
        a.drain_long_op();
        let after_g = a.active_tab().viewport_top;
        a.handle_key(key(KeyCode::Char('k')));
        assert!(
            a.active_tab().viewport_top < after_g,
            "viewport_top should retreat after k; was {after_g}, \
             now {} after one k press",
            a.active_tab().viewport_top,
        );
        dir.cleanup();
    }

    #[test]
    fn j_after_search_advances_viewport_top() {
        // User-reported regression: after `/<pattern><enter>` lands on
        // a match, subsequent `j` keystrokes appeared to do nothing.
        // Root cause: `search_step_forward` extends the streamview's
        // window only as far as needed to find the match.  Without
        // [`StreamView::ensure_window`]'s look-ahead pass, the anchor
        // lands at or near the cached window's back; the TUI's
        // `resync_from_streamview` clamps `viewport_top` to `max_top`
        // and `j` advances the anchor but the clamp pins
        // `viewport_top` so the visible content doesn't move.  Verify
        // by searching to a record near the back of the initial fetch
        // and confirming j-keystrokes actually shift `viewport_top`.
        let msgs: Vec<String> =
            (0..300).map(|i| format!("payload-m{i}")).collect();
        let records: Vec<(i64, &str)> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| (10 + i as i64, m.as_str()))
            .collect();
        let (mut a, dir) = host_app("test", &records);
        // Search for record 129 — close to but inside the streamview's
        // initial 138-line fetch.
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), r"payload-m129\b");
        a.handle_key(key(KeyCode::Enter));
        a.drain_long_op();

        let after_search = a.active_tab().viewport_top;
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('j')));
        assert!(
            a.active_tab().viewport_top > after_search,
            "viewport_top should advance after j-keystrokes; \
             was {after_search}, is now {} after 3 j presses",
            a.active_tab().viewport_top,
        );
        dir.cleanup();
    }

    #[test]
    fn slash_enter_repeats_last_search_forward() {
        let mut a = search_app();
        // Initial backward search.
        a.active_tab_mut().viewport_top = LineIdx(5);
        a.handle_key(key(KeyCode::Char('?')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.active_tab().viewport_top, 4);
        // Now /<enter>: same pattern, but Forward direction.
        a.handle_key(key(KeyCode::Char('/')));
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_none());
        // Forward repeat from row 4 advances to row 6.
        assert_eq!(a.active_tab().viewport_top, 6);
        // direction is now Forward.
        assert_eq!(
            a.last_search.as_ref().unwrap().direction,
            SearchDirection::Forward,
        );
    }

    #[test]
    fn question_enter_repeats_last_search_backward() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        a.handle_key(key(KeyCode::Enter));
        // viewport_top = 0.
        a.handle_key(key(KeyCode::Char('n')));
        a.handle_key(key(KeyCode::Char('n')));
        assert_eq!(a.active_tab().viewport_top, 4);
        a.handle_key(key(KeyCode::Char('?')));
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.active_tab().viewport_top, 2);
        assert_eq!(
            a.last_search.as_ref().unwrap().direction,
            SearchDirection::Backward,
        );
    }

    #[test]
    fn empty_search_with_no_history_is_noop() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_none());
        assert_eq!(a.active_tab().viewport_top, 0);
        assert!(a.last_search.is_none());
        assert!(a.active_tab().search.is_none());
    }

    #[test]
    fn n_with_no_history_is_noop() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('n')));
        a.handle_key(shift('N'));
        assert_eq!(a.active_tab().viewport_top, 0);
        assert!(a.last_search.is_none());
    }

    #[test]
    fn search_stays_put_when_no_match_in_direction() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "no_such_pattern");
        a.handle_key(key(KeyCode::Enter));
        // No matches; viewport doesn't move.
        assert_eq!(a.active_tab().viewport_top, 0);
        // tab.search still installed (with empty matches), so n is
        // also a no-op.
        a.handle_key(key(KeyCode::Char('n')));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn search_cleared_on_filter_change() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        a.handle_key(key(KeyCode::Enter));
        assert!(a.active_tab().search.is_some());
        // Apply any filter (with_rows uses an empty engine, so the
        // resulting rows will be empty — but that's fine for testing
        // that search is cleared).
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn");
        a.handle_key(key(KeyCode::Enter));
        assert!(a.active_tab().search.is_none());
        // last_search still remembered at the App level.
        assert!(a.last_search.is_some());
    }

    #[test]
    fn n_after_filter_change_re_derives_search() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        a.handle_key(key(KeyCode::Enter));
        // Advance once so the next `n` after re-derivation has a match
        // strictly past viewport_top.
        a.handle_key(key(KeyCode::Char('n')));
        assert_eq!(a.active_tab().viewport_top, 2);

        // Open the filter dialog with no edits and apply: rows
        // re-query (still all the same rows since with_rows engine has
        // no sources, so the re-queried rows are now empty), but the
        // search clears either way.  To realistically test
        // re-derivation, we sneak the rows back in directly.
        a.active_tab_mut().standalone_materialized.formatted = (0..10)
            .map(|i| {
                if i % 2 == 0 {
                    format!("alpha row {i}")
                } else {
                    format!("beta row {i}")
                }
            })
            .collect();
        a.active_tab_mut().search = None;
        a.active_tab_mut().viewport_top = LineIdx(3);

        a.handle_key(key(KeyCode::Char('n')));
        // Re-derives search; nearest match strictly past row 3 is 4.
        assert_eq!(a.active_tab().viewport_top, 4);
        assert!(a.active_tab().search.is_some());
    }

    #[test]
    fn compute_matches_returns_sorted_indices() {
        let rows: Vec<String> = ["foo", "bar", "foo bar", "baz", "qux foo"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let regex = Regex::new("foo").unwrap();
        assert_eq!(compute_matches(&rows, &regex), vec![0, 2, 4]);
    }

    #[test]
    fn highlight_line_styles_only_matched_runs() {
        let regex = Regex::new("alpha").unwrap();
        let line = highlight_line("xx alpha yy alpha zz", &regex);
        // Three plain spans, two reversed.
        assert_eq!(line.spans.len(), 5);
        let reversed = Style::default().add_modifier(Modifier::REVERSED);
        let plain = Style::default();
        assert_eq!(line.spans[0].style, plain);
        assert_eq!(line.spans[0].content, "xx ");
        assert_eq!(line.spans[1].style, reversed);
        assert_eq!(line.spans[1].content, "alpha");
        assert_eq!(line.spans[2].style, plain);
        assert_eq!(line.spans[2].content, " yy ");
        assert_eq!(line.spans[3].style, reversed);
        assert_eq!(line.spans[3].content, "alpha");
        assert_eq!(line.spans[4].style, plain);
        assert_eq!(line.spans[4].content, " zz");
    }

    #[test]
    fn highlight_line_skips_zero_width_matches() {
        // `a*` matches the empty string between every char — without
        // explicit handling this would emit a styled span of width 0
        // at every byte boundary.
        let regex = Regex::new("a*").unwrap();
        let line = highlight_line("xyz", &regex);
        // Only the unmatched-tail span survives.
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content, "xyz");
    }

    #[test]
    fn render_draws_search_prompt_at_bottom() {
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        // Bottom row should start with /alpha.  A 6-row screen lays
        // out: tab bar (1) + content (1+) + footer (1).  The prompt
        // replaces the footer.
        assert!(dump.contains("/alpha"), "dump:\n{dump}");
        // Footer text shouldn't be drawn while search is open.
        assert!(!dump.contains("q quit"), "dump:\n{dump}");
    }

    #[test]
    fn render_draws_search_parse_error_below_prompt() {
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "[");
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("/["), "dump:\n{dump}");
        assert!(
            dump.contains("regex parse error")
                || dump.contains("character class"),
            "expected a regex error:\n{dump}",
        );
    }

    #[test]
    fn render_highlights_matches_in_visible_rows() {
        let backend = TestBackend::new(40, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["alpha line".to_string()]);
        a.viewport_height = 1;
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        a.handle_key(key(KeyCode::Enter));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let buf = terminal.backend().buffer();
        // Layout: row 0 is the tab bar, row 1 is the first content row.
        // Cells (0..5, 1) cover "alpha" and should be reversed; cell
        // (5, 1) is the space after, plain.
        for x in 0..5 {
            assert!(
                buf[(x, 1)].modifier.contains(Modifier::REVERSED),
                "expected REVERSED at ({x}, 1)",
            );
        }
        assert!(
            !buf[(5, 1)].modifier.contains(Modifier::REVERSED),
            "expected plain at (5, 1)",
        );
    }

    // ---------- exclude mode ----------

    /// Builds a fixture event with a custom `msg`.  Other bunyan-core
    /// fields take fixed values that the exclude tests don't inspect.
    fn ev(msg: &str) -> LogEvent {
        let json = format!(
            r#"{{
                "v": 0,
                "level": 30,
                "name": "Nexus",
                "hostname": "sled-01",
                "pid": 1234,
                "time": "2025-04-01T00:00:00Z",
                "msg": {}
            }}"#,
            serde_json::Value::String(msg.to_string()),
        );
        serde_json::from_str(&json).unwrap()
    }

    /// Constructs an App with `n` event rows whose msgs are "msg 0",
    /// "msg 1", ... and a viewport_height of `h`.
    fn select_app(n: usize, h: u16) -> App {
        let events: Vec<Option<LogEvent>> =
            (0..n).map(|i| Some(ev(&format!("msg {i}")))).collect();
        let formatted: Vec<String> =
            (0..n).map(|i| format!("row {i}")).collect();
        let mut a = App::with_events(events, formatted);
        a.viewport_height = h;
        a
    }

    fn excl_sel(event_idx: usize) -> Selection {
        Selection {
            event_idx: EventIdx(event_idx),
            action: SelectionAction::Exclude,
        }
    }

    fn incl_sel(event_idx: usize) -> Selection {
        Selection {
            event_idx: EventIdx(event_idx),
            action: SelectionAction::Include,
        }
    }

    fn bm_sel(event_idx: usize) -> Selection {
        Selection {
            event_idx: EventIdx(event_idx),
            action: SelectionAction::Bookmark,
        }
    }

    #[test]
    fn x_enters_exclude_mode_at_viewport_top() {
        let mut a = select_app(10, 5);
        a.active_tab_mut().viewport_top = LineIdx(3);
        a.handle_key(key(KeyCode::Char('x')));
        assert_eq!(a.active_tab().select, Some(excl_sel(3)));
    }

    #[test]
    fn x_is_noop_when_no_rows() {
        let mut a = App::with_rows(Vec::new());
        a.viewport_height = 5;
        a.handle_key(key(KeyCode::Char('x')));
        assert_eq!(a.active_tab().select, None);
    }

    #[test]
    fn shift_x_enters_include_mode_at_viewport_top() {
        let mut a = select_app(10, 5);
        a.active_tab_mut().viewport_top = LineIdx(3);
        a.handle_key(shift('X'));
        assert_eq!(a.active_tab().select, Some(incl_sel(3)));
    }

    #[test]
    fn shift_x_accepts_no_modifier_form() {
        // Some terminals drop the SHIFT modifier on capital letters.
        let mut a = select_app(10, 5);
        a.handle_key(key(KeyCode::Char('X')));
        assert_eq!(a.active_tab().select, Some(incl_sel(0)));
    }

    #[test]
    fn select_j_moves_selection_within_viewport() {
        let mut a = select_app(10, 5);
        a.handle_key(key(KeyCode::Char('x')));
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.active_tab().select, Some(excl_sel(1)));
        assert_eq!(a.active_tab().viewport_top, 0);
        a.handle_key(key(KeyCode::Down));
        assert_eq!(a.active_tab().select, Some(excl_sel(2)));
    }

    #[test]
    fn select_k_moves_selection_up() {
        let mut a = select_app(10, 5);
        a.handle_key(key(KeyCode::Char('x')));
        a.active_tab_mut().select = Some(excl_sel(3));
        a.handle_key(key(KeyCode::Char('k')));
        assert_eq!(a.active_tab().select, Some(excl_sel(2)));
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.active_tab().select, Some(excl_sel(1)));
    }

    #[test]
    fn select_motion_preserves_polarity() {
        // Entering with `X` and then moving must not silently flip the
        // selection back to "exclude".
        let mut a = select_app(10, 5);
        a.handle_key(shift('X'));
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.active_tab().select, Some(incl_sel(1)));
    }

    #[test]
    fn select_j_past_viewport_bottom_scrolls() {
        let mut a = select_app(10, 3);
        a.handle_key(key(KeyCode::Char('x')));
        // Selection starts at 0, viewport [0..3).  Three j's land
        // selection at 3, which is below the viewport — viewport
        // should scroll to keep it just inside.
        for _ in 0..3 {
            a.handle_key(key(KeyCode::Char('j')));
        }
        assert_eq!(a.active_tab().select, Some(excl_sel(3)));
        assert_eq!(a.active_tab().viewport_top, 1);
    }

    #[test]
    fn select_k_above_viewport_top_scrolls() {
        let mut a = select_app(10, 3);
        a.active_tab_mut().viewport_top = LineIdx(5);
        a.handle_key(key(KeyCode::Char('x')));
        // Selection starts at viewport_top (5), one `k` puts it at 4
        // which is above the current viewport — viewport_top should
        // follow.
        a.handle_key(key(KeyCode::Char('k')));
        assert_eq!(a.active_tab().select, Some(excl_sel(4)));
        assert_eq!(a.active_tab().viewport_top, 4);
    }

    #[test]
    fn select_clamps_at_ends() {
        let mut a = select_app(3, 5);
        a.handle_key(key(KeyCode::Char('x')));
        a.handle_key(key(KeyCode::Char('k')));
        assert_eq!(a.active_tab().select, Some(excl_sel(0)));
        for _ in 0..10 {
            a.handle_key(key(KeyCode::Char('j')));
        }
        assert_eq!(a.active_tab().select, Some(excl_sel(2)));
    }

    #[test]
    fn select_esc_cancels_without_changing_filter() {
        let mut a = select_app(5, 5);
        let before = &a.active_stream().filter.to_string();
        a.handle_key(key(KeyCode::Char('x')));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.active_tab().select, None);
        assert_eq!(&a.active_stream().filter.to_string(), before);
    }

    #[test]
    fn exclude_enter_appends_negated_predicate_and_exits_mode() {
        let mut a = select_app(5, 5);
        a.handle_key(key(KeyCode::Char('x')));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('j')));
        // Selection is at row 2 → "msg 2".
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.active_tab().select, None);
        // The new predicate displays as `msg!="msg 2"` (quoted because
        // it contains whitespace).
        let displayed = &a.active_stream().filter.to_string();
        assert!(
            displayed.contains("msg!=") && displayed.contains("msg 2"),
            "expected exclusion predicate in {displayed:?}",
        );
    }

    #[test]
    fn include_enter_appends_positive_predicate_and_exits_mode() {
        let mut a = select_app(5, 5);
        a.handle_key(shift('X'));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('j')));
        // Selection is at row 2 → "msg 2".
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.active_tab().select, None);
        let displayed = &a.active_stream().filter.to_string();
        assert!(
            displayed.contains("msg=") && displayed.contains("msg 2"),
            "expected inclusion predicate in {displayed:?}",
        );
        // Sanity: not the negated form.
        assert!(
            !displayed.contains("msg!="),
            "include must not produce a negated predicate: {displayed:?}",
        );
    }

    #[test]
    fn select_enter_on_error_row_is_noop() {
        // Two real events sandwich one error row at index 1.
        let events = vec![Some(ev("first")), None, Some(ev("third"))];
        let formatted = vec![
            "row 0".to_string(),
            "error: parse failure".to_string(),
            "row 2".to_string(),
        ];
        let mut a = App::with_events(events, formatted);
        a.viewport_height = 5;
        a.handle_key(key(KeyCode::Char('x')));
        a.handle_key(key(KeyCode::Char('j'))); // selection -> row 1 (error)
        let before = &a.active_stream().filter.to_string();
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.active_tab().select, Some(excl_sel(1)));
        assert_eq!(&a.active_stream().filter.to_string(), before);
    }

    #[test]
    fn select_other_keys_are_ignored() {
        let mut a = select_app(10, 5);
        a.handle_key(key(KeyCode::Char('x')));
        // `q` would normally quit; in select mode it's swallowed.
        a.handle_key(key(KeyCode::Char('q')));
        assert!(!a.quit);
        assert_eq!(a.active_tab().select, Some(excl_sel(0)));
        // `f` would normally open the filter dialog; not in this mode.
        a.handle_key(key(KeyCode::Char('f')));
        assert!(a.dialog.is_none());
        // Tab switching also disabled.
        a.handle_key(key(KeyCode::Tab));
        assert_eq!(a.active, 0);
    }

    #[test]
    fn render_paints_select_highlight() {
        // Tall enough to fit the tab bar, two content rows, the parse
        // stats row, and a footer.  Selection is on row 1 (the second
        // event).
        let backend = TestBackend::new(20, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = select_app(3, 2);
        a.handle_key(key(KeyCode::Char('x')));
        a.handle_key(key(KeyCode::Char('j')));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let buf = terminal.backend().buffer();
        // Layout: row 0 = tab bar, row 1 = first content row, row 2 =
        // second content row (the selected one), row 3 = parse stats,
        // row 4 = footer.
        assert!(
            buf[(0, 2)].style().bg == Some(Color::DarkGray),
            "expected DarkGray bg on selected content row",
        );
        assert!(
            buf[(0, 1)].style().bg != Some(Color::DarkGray),
            "non-selected content row should not be highlighted",
        );
    }

    #[test]
    fn render_shows_exclude_footer_hint() {
        let backend = TestBackend::new(80, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = select_app(3, 2);
        a.handle_key(key(KeyCode::Char('x')));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(
            dump.contains("Enter exclude msg"),
            "expected exclude-mode footer hint, got:\n{dump}",
        );
    }

    #[test]
    fn render_shows_include_footer_hint() {
        let backend = TestBackend::new(80, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = select_app(3, 2);
        a.handle_key(shift('X'));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(
            dump.contains("Enter include msg"),
            "expected include-mode footer hint, got:\n{dump}",
        );
    }

    // ---------- parse stats ----------

    #[test]
    fn format_bytes_picks_prefix_at_1024_boundaries() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        // 1.5 KiB
        assert_eq!(format_bytes(1024 + 512), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GiB");
        // The 2013-byte example from the user prompt: well above the
        // 1 KiB boundary, so we shift to KiB rather than printing raw
        // bytes.
        assert_eq!(format_bytes(2013), "2.0 KiB");
    }

    #[test]
    fn format_byte_rate_picks_prefix_at_1024_boundaries() {
        assert_eq!(format_byte_rate(0.0), "0 B/sec");
        assert_eq!(format_byte_rate(132.0), "132 B/sec");
        assert_eq!(format_byte_rate(1024.0), "1.0 KiB/sec");
        assert_eq!(format_byte_rate(1024.0 * 1024.0), "1.0 MiB/sec");
    }

    #[test]
    fn format_fetch_stats_includes_records_bytes_time_and_rates() {
        let stats = ParseStats {
            records: 1023,
            walked_bytes: ByteLen::from(2013),
            elapsed: Duration::from_millis(15_231),
        };
        let s = format_fetch_stats(&stats);
        // Spot-check each piece of information the user expects to see.
        assert!(s.contains("1023 records"), "{s}");
        assert!(s.contains("2.0 KiB"), "{s}");
        assert!(s.contains("15.231s"), "{s}");
        assert!(s.contains("fetched"), "{s}");
        assert!(s.contains("records/sec"), "{s}");
        assert!(s.contains("B/sec") || s.contains("KiB/sec"), "{s}");
    }

    #[test]
    fn format_fetch_stats_drops_rates_when_records_zero() {
        // Empty engine (no sources or all filtered out): records and
        // bytes are zero and the rate half would be meaningless.
        let stats = ParseStats {
            records: 0,
            walked_bytes: ByteLen::ZERO,
            elapsed: Duration::from_millis(0),
        };
        let s = format_fetch_stats(&stats);
        assert!(s.contains("0 records"), "{s}");
        assert!(s.contains("fetched"), "{s}");
        assert!(!s.contains("records/sec"), "{s}");
    }

    /// Builds a small bunyan log file, points an Engine at it, and
    /// returns the App so a test can drive the streamview-backed
    /// status-line code path that needs real byte offsets.
    fn app_with_real_log(records: usize) -> (App, TestDir) {
        use slog::{Drain, Logger, info, o};
        use std::fs::OpenOptions;
        use std::sync::Mutex;

        let dir = TestDir::new();
        let path = dir.path().join("a.log");
        {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            let drain = slog_bunyan::with_name("Nexus", file).build().fuse();
            let log = Logger::root(Mutex::new(drain).fuse(), o!());
            for i in 0..records {
                info!(log, "entry"; "i" => i);
            }
        }
        let mut engine = Engine::new();
        engine.add_file_source(&path).unwrap();
        (App::new_for_tests(engine), dir)
    }

    #[test]
    fn user_status_shows_byte_offset_and_percent_when_streamview_present() {
        // A real file-backed engine is required so the streamview's
        // anchor cursor has meaningful byte offsets to sum.  At the
        // top of the stream the offset is zero, the percent rounds to
        // 0, and the "(beginning of stream)" marker appears.
        let (mut a, dir) = app_with_real_log(50);
        a.viewport_height = 5;
        let backend = TestBackend::new(160, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("Showing"), "dump:\n{dump}");
        assert!(dump.contains("from byte offset"), "dump:\n{dump}");
        assert!(dump.contains("0 B of"), "dump:\n{dump}");
        assert!(dump.contains("(0%)"), "dump:\n{dump}");
        assert!(dump.contains("(beginning of stream)"), "dump:\n{dump}");
        dir.cleanup();
    }

    #[test]
    fn user_status_shows_end_marker_at_eof() {
        // Scrolling to the end of a streamview-backed tab surfaces the
        // "(end of stream)" marker and a non-zero byte offset.  The
        // beginning-of-stream marker must not also appear.
        let (mut a, dir) = app_with_real_log(50);
        a.viewport_height = 10;
        let backend = TestBackend::new(160, 13);
        let mut terminal = Terminal::new(backend).unwrap();
        // `G` installs a Seek long-op; the user status line is taken
        // over by the progress bar while it runs, so we have to drain
        // it before checking for the EOF marker.
        a.handle_key(shift('G'));
        a.drain_long_op();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("(end of stream)"), "dump:\n{dump}");
        assert!(!dump.contains("(beginning of stream)"), "dump:\n{dump}");
        assert!(dump.contains("from byte offset"), "dump:\n{dump}");
        dir.cleanup();
    }

    #[test]
    fn user_status_byte_offset_reflects_filter_skipped_bytes() {
        // Pins the user-reported bug: applying a filter that excludes
        // the start of the file used to leave the byte offset at 0,
        // because `front_cursor` wasn't anchored to the first match.
        // After the fix, the user status reports a non-zero offset
        // corresponding to the first matching record's actual byte
        // position in its source file.
        let (mut a, dir) = app_with_real_log(50);
        a.viewport_height = 5;
        let backend = TestBackend::new(160, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        // Filter to a record well into the file.  `apply_filter`
        // installs a Seek long op that does the work; drain it before
        // rendering so the user status line (not the progress bar) is
        // the one we read.
        let filter: Filter = "msg=entry  i=40".parse().unwrap();
        a.apply_filter(filter);
        a.drain_long_op();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("from byte offset"), "dump:\n{dump}");
        // The "0 B of" form is what showed before the fix; the offset
        // should now be the actual position of the first matching
        // record, which has to be more than a hundred bytes deep into
        // a 50-record bunyan log.
        assert!(!dump.contains("from byte offset 0 B of"), "dump:\n{dump}");
        // ...and no longer marked as the beginning of the stream.
        assert!(!dump.contains("(beginning of stream)"), "dump:\n{dump}");
        dir.cleanup();
    }

    #[test]
    fn user_status_byte_offset_accounts_for_filtered_sister_sources() {
        // Repros the multi-source case the user reported against `t/`:
        // three per-sled log files, the user filters out one sled's
        // hostname, and the displayed byte offset must reflect the
        // bytes the engine walked past in the filtered-out sources
        // (not just the visible record's offset in its own file).
        use std::io::Write;

        let dir = TestDir::new();
        let paths: Vec<_> = (0..3)
            .map(|i| dir.path().join(format!("sled-{i:02}.log")))
            .collect();
        // Hand-roll the bunyan records: slog-bunyan auto-injects the
        // machine `hostname`, which clashes with our per-sled
        // `hostname` field and would render the filter a no-op.
        for (i, path) in paths.iter().enumerate() {
            let mut file = std::fs::File::create(path).unwrap();
            for j in 0..50 {
                let secs = (i as i64) * 1000 + i64::from(j);
                writeln!(
                    file,
                    r#"{{"hostname":"oxz-sled-{i:02}.oxide.test","level":30,"msg":"entry","name":"SledAgent","pid":1234,"time":"2024-03-09T16:{:02}:{:02}+00:00","v":0,"j":{j}}}"#,
                    secs / 60,
                    secs % 60,
                )
                .unwrap();
            }
        }
        let mut engine = Engine::new();
        for path in &paths {
            engine.add_file_source(path).unwrap();
        }
        let mut a = App::new_for_tests(engine);
        a.viewport_height = 5;

        // Filter out the first sled entirely: every one of its records
        // matches the hostname predicate's *exclusion*, and the merge
        // stepper has to walk past every byte of sled-00's file to
        // satisfy the time-merge before it can return a sled-01 record.
        let filter: Filter =
            "hostname!=oxz-sled-00.oxide.test".parse().unwrap();
        a.apply_filter(filter);
        a.drain_long_op();

        let backend = TestBackend::new(160, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("from byte offset"), "dump:\n{dump}");
        // sled-00's file is large enough that we should observe a
        // non-trivial offset, not 0.  The user's bug had the line
        // reading `from byte offset 0 B of …` even after the filter.
        assert!(
            !dump.contains("from byte offset 0 B of"),
            "byte offset should reflect bytes walked past in the \
             filtered-out sister source, dump:\n{dump}",
        );
        assert!(
            !dump.contains("(beginning of stream)"),
            "should not still be marked as beginning, dump:\n{dump}",
        );
        dir.cleanup();
    }

    #[test]
    fn user_status_byte_offset_survives_k_at_top_of_filtered_stream() {
        // Pins a second-order bug surfaced after the multi-source
        // fix: after applying a filter that excludes a whole source,
        // pressing `k` at the top of the stream (a no-op
        // navigation-wise) used to reset the user-status byte offset
        // back to 0 — the backward extend's stepper walked the
        // excluded source down to 0 and the streamview blindly
        // overwrote `front_cursor` with the now-zero stepper cursor.
        use std::io::Write;

        let dir = TestDir::new();
        let paths: Vec<_> = (0..3)
            .map(|i| dir.path().join(format!("sled-{i:02}.log")))
            .collect();
        for (i, path) in paths.iter().enumerate() {
            let mut file = std::fs::File::create(path).unwrap();
            for j in 0..50 {
                let secs = (i as i64) * 1000 + i64::from(j);
                writeln!(
                    file,
                    r#"{{"hostname":"oxz-sled-{i:02}.oxide.test","level":30,"msg":"entry","name":"SledAgent","pid":1234,"time":"2024-03-09T16:{:02}:{:02}+00:00","v":0,"j":{j}}}"#,
                    secs / 60,
                    secs % 60,
                )
                .unwrap();
            }
        }
        let mut engine = Engine::new();
        for path in &paths {
            engine.add_file_source(path).unwrap();
        }
        let mut a = App::new_for_tests(engine);
        a.viewport_height = 5;

        let filter: Filter =
            "hostname!=oxz-sled-00.oxide.test".parse().unwrap();
        a.apply_filter(filter);
        a.drain_long_op();

        let backend = TestBackend::new(160, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let before = buffer_text(terminal.backend().buffer());
        assert!(
            !before.contains("from byte offset 0 B of"),
            "post-filter offset should be non-zero, dump:\n{before}",
        );

        // `k` at the top: viewport doesn't actually scroll (no records
        // earlier than records[0] exist under the filter), so the
        // user status should be unchanged.
        a.handle_key(key(KeyCode::Char('k')));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let after = buffer_text(terminal.backend().buffer());
        assert!(
            !after.contains("from byte offset 0 B of"),
            "k at top should not have reset the byte offset to 0, \
             before:\n{before}\nafter:\n{after}",
        );
        dir.cleanup();
    }

    #[test]
    fn user_status_for_synthetic_fixture_omits_byte_half() {
        // Tabs without a streamview (synthetic test fixtures and
        // pre-build Summary tabs) carry no byte-offset signal, so the
        // user status line drops the "from byte offset …" half rather
        // than printing a misleading zero.
        let backend = TestBackend::new(120, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("Showing 1 records"), "dump:\n{dump}");
        assert!(!dump.contains("from byte offset"), "dump:\n{dump}");
        // The synthetic path has no streamview, so the beginning
        // marker should not appear even though the viewport is at
        // line 0 of the materialization.
        assert!(!dump.contains("(beginning of stream)"), "dump:\n{dump}");
    }

    #[test]
    fn render_shows_user_status_above_footer_by_default() {
        // Wide enough to hold the full status line; tall enough for
        // tabs(1) + content(2) + stats(1) + footer(1).  The user
        // status row sits one above the footer.  The developer-only
        // fetch-stats row is hidden by default, so the user just
        // sees the "Showing N records …" line.
        let backend = TestBackend::new(120, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.active_tab_mut().standalone_materialized.parse_stats = ParseStats {
            records: 42,
            walked_bytes: ByteLen::from(4096),
            elapsed: Duration::from_millis(100),
        };
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("Showing 1 records"), "dump:\n{dump}");
        // No fetch-stats row in default state — the "42 records …
        // fetched" text must not appear anywhere on screen.
        assert!(!dump.contains("fetched"), "dump:\n{dump}");
    }

    #[test]
    fn p_reveals_fetch_stats_row() {
        // Pressing `p` toggles the developer-oriented fetch-stats row
        // on.  The row shows the engine's running ParseStats with the
        // user-friendly "fetched" verb.  Tall enough for
        // tabs(1) + content(1) + fetch(1) + stats(1) + footer(1).
        let backend = TestBackend::new(160, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.active_tab_mut().standalone_materialized.parse_stats = ParseStats {
            records: 42,
            walked_bytes: ByteLen::from(4096),
            elapsed: Duration::from_millis(100),
        };
        a.handle_key(key(KeyCode::Char('p')));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("42 records"), "dump:\n{dump}");
        assert!(dump.contains("fetched"), "dump:\n{dump}");
        assert!(dump.contains("4.0 KiB"), "dump:\n{dump}");
        assert!(dump.contains("0.100s"), "dump:\n{dump}");
        // The user status line still shows in its slot.
        assert!(dump.contains("Showing 1 records"), "dump:\n{dump}");
    }

    #[test]
    fn p_toggle_hides_fetch_stats_row_again() {
        // Press `p` twice: row appears, then disappears.  Verifies the
        // toggle is a true flip-flop, not a one-shot.
        let backend = TestBackend::new(160, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["row".to_string()]);
        a.active_tab_mut().standalone_materialized.parse_stats = ParseStats {
            records: 42,
            walked_bytes: ByteLen::from(4096),
            elapsed: Duration::from_millis(100),
        };
        a.handle_key(key(KeyCode::Char('p')));
        a.handle_key(key(KeyCode::Char('p')));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(!dump.contains("fetched"), "dump:\n{dump}");
    }

    // ---------- progress bar ----------

    #[test]
    fn progress_bar_inner_one_cell_when_pct_positive_but_low() {
        // A 1% progress bar over 100 cells would round down to zero
        // filled cells; in that case we still show one filled cell so
        // the user can see *something* moved.
        let s = progress_bar_inner(0.5, 100);
        assert!(s.starts_with('\u{2588}'), "got: {s:?}");
        assert_eq!(s.chars().count(), 100);
    }

    #[test]
    fn progress_bar_inner_full_at_100() {
        let s = progress_bar_inner(100.0, 10);
        assert_eq!(s, "\u{2588}".repeat(10));
    }

    #[test]
    fn progress_bar_inner_empty_at_zero() {
        // 0% progress paints an all-blank bar (no minimum-cell bump,
        // since there really is no progress yet).
        let s = progress_bar_inner(0.0, 10);
        assert_eq!(s, " ".repeat(10));
    }

    #[test]
    fn format_long_op_progress_includes_label_pct_bytes_records() {
        // Build a SummaryOp whose state is set up by hand so the
        // formatted output is deterministic — we don't drive a real
        // engine here.
        let mut s =
            SummaryOp::new(TabIdx(0), Filter::default(), ByteLen::from(1024));
        s.bytes_read = ByteLen::from(256);
        s.records = 42;
        let op = LongOp::BuildSummary(Box::new(s));
        let out = format_long_op_progress(&op, 80);
        assert!(out.contains("Computing summary"), "{out}");
        assert!(out.contains("25.0%"), "{out}");
        assert!(out.contains("256 B"), "{out}");
        assert!(out.contains("1.0 KiB"), "{out}");
        assert!(out.contains("42 records"), "{out}");
        // The bar should fit alongside the numeric components on an
        // 80-column terminal.
        assert!(out.contains('['), "{out}");
        assert!(out.contains(']'), "{out}");
    }

    #[test]
    fn format_long_op_progress_drops_bar_at_narrow_widths() {
        // At 30 cells there isn't room for a useful bar plus the
        // numbers; the bar is dropped rather than truncating the
        // numbers.
        let mut s =
            SummaryOp::new(TabIdx(0), Filter::default(), ByteLen::from(1024));
        s.bytes_read = ByteLen::from(256);
        s.records = 42;
        let op = LongOp::BuildSummary(Box::new(s));
        let out = format_long_op_progress(&op, 30);
        assert!(out.contains("25.0%"), "{out}");
        assert!(out.contains("42 records"), "{out}");
        assert!(!out.contains('['), "expected no bar; got: {out}");
    }

    #[test]
    fn format_long_op_progress_caps_at_100() {
        // Search ops can overshoot total_bytes when the streamview
        // had already pulled bytes for back-fetch that we count
        // against the same denominator; the bar should clamp to 100%.
        let mut s =
            SummaryOp::new(TabIdx(0), Filter::default(), ByteLen::from(100));
        s.bytes_read = ByteLen::from(200);
        s.records = 1;
        let op = LongOp::BuildSummary(Box::new(s));
        let out = format_long_op_progress(&op, 80);
        assert!(out.contains("100.0%"), "{out}");
    }

    #[test]
    fn long_op_drives_summary_to_completion() {
        // End-to-end: pressing `S` installs a placeholder, then the
        // long op chunks through the merged stream until the summary
        // is fully built.  After draining, the rendered rows should
        // match what the synchronous build produced previously.
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[]),
            (20, "first", &[]),
            (30, "second", &[]),
        ]);
        a.handle_key(shift('S'));
        // Before draining: placeholder lives in the tab, long op is
        // active, progress is reported on the active tab's stats row.
        assert_eq!(
            a.active_tab().formatted(),
            &["Computing summary...".to_string()],
        );
        assert!(a.long_op.is_some());
        a.drain_long_op();
        assert!(a.long_op.is_none());
        let lines = a.active_tab().formatted();
        assert!(
            lines.iter().any(|l| l.starts_with("Summary: 3 events")),
            "got:\n{}",
            lines.join("\n"),
        );
        dir.cleanup();
    }

    #[test]
    fn long_op_summary_cancel_leaves_placeholder_and_clears_queue() {
        let (mut a, dir) =
            multi_line_app(&[(10, "first", &[]), (20, "second", &[])]);
        a.handle_key(shift('S'));
        assert!(matches!(a.long_op, Some(LongOp::BuildSummary(_))));
        a.cancel_long_op();
        assert!(a.long_op.is_none());
        assert!(a.pending_summary_builds.is_empty());
        let lines = a.active_tab().formatted();
        assert!(
            lines.iter().any(|l| l.contains("cancelled")),
            "expected cancel notice, got: {lines:?}"
        );
        dir.cleanup();
    }

    #[test]
    fn long_op_summary_filter_change_supersedes_pending() {
        // A second build for the same tab while the first is still
        // pending should drop the older request — only the newest
        // filter is honored.
        let (mut a, dir) =
            multi_line_app(&[(10, "alpha", &[]), (20, "beta", &[])]);
        a.handle_key(shift('S'));
        // Force a pending state by stalling the active op without
        // finishing it: spin a no-op LongOp::Search would be wrong;
        // just enqueue another build for the same tab while the
        // current one is still in flight.  enqueue_summary_build
        // de-dupes pending entries for the same tab.
        let active_tab = TabIdx(a.active);
        a.pending_summary_builds
            .push_back((active_tab, "msg=alpha".parse().unwrap()));
        a.enqueue_summary_build(active_tab, "msg=beta".parse().unwrap());
        assert_eq!(a.pending_summary_builds.len(), 1);
        let (_, last) = a.pending_summary_builds.front().unwrap();
        assert_eq!(last.to_string(), "msg=beta");
        dir.cleanup();
    }

    #[test]
    fn render_shows_progress_bar_while_long_op_active() {
        // Pressing `S` installs a placeholder + long op; the very
        // next frame should show the progress bar in place of the
        // parse-stats row.
        let (mut a, dir) =
            multi_line_app(&[(10, "first", &[]), (20, "second", &[])]);
        a.handle_key(shift('S'));
        let backend = TestBackend::new(120, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(
            dump.contains("Computing summary"),
            "expected progress bar; dump:\n{dump}",
        );
        dir.cleanup();
    }

    #[test]
    fn long_op_search_drives_through_streamview() {
        // A `/` search on a real engine-backed tab spins up a Search
        // long op that the main loop drives.  After draining, the
        // viewport should land on the matching record.  A 30-row
        // fixture with a small viewport guarantees the post-search
        // `viewport_top` actually has somewhere to scroll to (a
        // 4-row fixture would clamp to 0 regardless of the match
        // location).
        let mut records: Vec<RecordSpec<'_>> = (0..30)
            .map(|i| (10 + i64::from(i) * 10, "filler", &[][..]))
            .collect();
        records[25] = (260, "match-here", &[]);
        let (mut a, dir) = multi_line_app(&records);
        a.viewport_height = 5;
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "match-here");
        a.handle_key(key(KeyCode::Enter));
        // Search now in flight via long_op.
        assert!(matches!(a.long_op, Some(LongOp::Search(_))));
        a.drain_long_op();
        assert!(a.long_op.is_none());
        // Viewport should sit on the matching record.
        let top = a.active_tab().viewport_top.get();
        let line = &a.active_tab().formatted()[top];
        assert!(
            line.contains("match-here"),
            "expected viewport on the match; got line {line:?} \
             (top={top}, formatted.len()={})",
            a.active_tab().formatted().len(),
        );
        dir.cleanup();
    }

    // ---------- end progress bar ----------

    #[test]
    fn render_omits_parse_stats_row_on_bookmarks_pane() {
        // The bookmarks pane has no parse activity; the stats row
        // collapses to 0 height there so the bookmark list keeps the
        // full content area.
        let mut a = select_app(5, 5);
        create_bookmark(&mut a, 0, Some("first"));
        a.handle_key(key(KeyCode::Tab));
        assert!(a.bookmarks_active());
        // Make the active *regular* tab's stats distinctive — they
        // should not appear when the bookmarks pane is showing.
        a.tabs[0].standalone_materialized.parse_stats = ParseStats {
            records: 9999,
            walked_bytes: ByteLen::from(1024 * 1024),
            elapsed: Duration::from_millis(1234),
        };
        let backend = TestBackend::new(120, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(!dump.contains("9999 records"), "dump:\n{dump}");
    }

    // ---------- time-step navigation ----------

    /// Builds a fixture event with a specific timestamp (in epoch
    /// seconds) and msg.  Counterpart to [`ev`] which always pins the
    /// time to a fixed value; the time-step tests need controllable
    /// spacing between rows.
    fn ev_at(secs: i64, msg: &str) -> LogEvent {
        let time = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
            .unwrap()
            .to_rfc3339();
        let json = format!(
            r#"{{
                "v": 0,
                "level": 30,
                "name": "Nexus",
                "hostname": "sled-01",
                "pid": 1234,
                "time": "{}",
                "msg": {}
            }}"#,
            time,
            serde_json::Value::String(msg.to_string()),
        );
        serde_json::from_str(&json).unwrap()
    }

    /// App with one event per second from 0..n.
    fn time_app(n: i64, h: u16) -> App {
        let events: Vec<Option<LogEvent>> =
            (0..n).map(|i| Some(ev_at(i, &format!("at {i}")))).collect();
        let formatted: Vec<String> =
            (0..n).map(|i| format!("row {i}")).collect();
        let mut a = App::with_events(events, formatted);
        a.viewport_height = h;
        a
    }

    #[test]
    fn default_step_is_one_minute() {
        let a = app(0, 5);
        assert_eq!(a.current_step_label(), "1m");
    }

    #[test]
    fn equals_increases_step_clamped_at_top() {
        let mut a = app(0, 5);
        for _ in 0..TIME_STEPS.len() {
            a.handle_key(key(KeyCode::Char('=')));
        }
        // Clamps at the largest entry rather than wrapping.
        assert_eq!(a.current_step_label(), TIME_STEPS.last().unwrap().0);
        a.handle_key(key(KeyCode::Char('=')));
        assert_eq!(a.current_step_label(), TIME_STEPS.last().unwrap().0);
    }

    #[test]
    fn plus_is_alias_for_increase() {
        let mut a = app(0, 5);
        let before = a.time_step_idx;
        a.handle_key(shift('+'));
        assert_eq!(a.time_step_idx, before + 1);
        // Same key without the SHIFT modifier — some terminals.
        a.handle_key(key(KeyCode::Char('+')));
        assert_eq!(a.time_step_idx, before + 2);
    }

    #[test]
    fn minus_decreases_step_clamped_at_bottom() {
        let mut a = app(0, 5);
        for _ in 0..TIME_STEPS.len() {
            a.handle_key(key(KeyCode::Char('-')));
        }
        assert_eq!(a.current_step_label(), TIME_STEPS[0].0);
        a.handle_key(key(KeyCode::Char('-')));
        assert_eq!(a.current_step_label(), TIME_STEPS[0].0);
    }

    #[test]
    fn gt_lands_on_first_event_past_target() {
        // 120 rows at 1s each; +1m from row 0 lands exactly on row 60.
        let mut a = time_app(120, 10);
        a.handle_key(shift('>'));
        assert_eq!(a.active_tab().viewport_top, 60);
        // Repeat: +1m from row 60 → row 120; clamped to max_top.
        a.handle_key(shift('>'));
        let max = a.active_tab().max_top(a.viewport_height, a.viewport_width);
        assert_eq!(a.active_tab().viewport_top, max);
    }

    #[test]
    fn gt_snaps_to_end_when_no_event_past_target() {
        // 60 events at 1s; +1m from row 0 has no event at >= t=60.
        let mut a = time_app(60, 10);
        a.handle_key(shift('>'));
        let max = a.active_tab().max_top(a.viewport_height, a.viewport_width);
        assert_eq!(a.active_tab().viewport_top, max);
    }

    #[test]
    fn gt_accepts_no_modifier_form() {
        // Some terminals report `>` with no SHIFT modifier even though
        // it's typed with shift.
        let mut a = time_app(120, 10);
        a.handle_key(key(KeyCode::Char('>')));
        assert_eq!(a.active_tab().viewport_top, 60);
    }

    #[test]
    fn lt_rewinds_by_current_step() {
        // Start at row 90 (t=90s); -1m lands at t=30s → row 30.
        let mut a = time_app(120, 10);
        a.active_tab_mut().viewport_top = LineIdx(90);
        a.handle_key(shift('<'));
        assert_eq!(a.active_tab().viewport_top, 30);
        // Another `<` from t=30 → t=-30, no event satisfies → top.
        a.handle_key(shift('<'));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn step_change_then_advance_uses_new_step() {
        // 30 rows at 1s.  Drop step to 5s, then `>` should land at
        // row 5 (t=5s).
        let mut a = time_app(30, 10);
        // Default is "1m" (idx 5); four `-` presses → "1s" (idx 1).
        for _ in 0..4 {
            a.handle_key(key(KeyCode::Char('-')));
        }
        assert_eq!(a.current_step_label(), "1s");
        // One step up → "5s".
        a.handle_key(key(KeyCode::Char('=')));
        assert_eq!(a.current_step_label(), "5s");
        a.handle_key(shift('>'));
        assert_eq!(a.active_tab().viewport_top, 5);
    }

    #[test]
    fn advance_with_no_events_is_noop() {
        let mut a = App::with_events(Vec::new(), Vec::new());
        a.viewport_height = 5;
        a.handle_key(shift('>'));
        a.handle_key(shift('<'));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn advance_with_only_error_rows_is_noop() {
        // No parsed events means there's no anchor time to add the
        // step to; both `>` and `<` should be no-ops rather than
        // jumping to an arbitrary row.
        let events = vec![None, None, None];
        let formatted = vec!["err 0".into(), "err 1".into(), "err 2".into()];
        let mut a = App::with_events(events, formatted);
        a.viewport_height = 5;
        a.active_tab_mut().viewport_top = LineIdx(1);
        a.handle_key(shift('>'));
        a.handle_key(shift('<'));
        assert_eq!(a.active_tab().viewport_top, 1);
    }

    #[test]
    fn advance_anchors_through_error_row() {
        // Events at t=0, [error], t=120s.  Park viewport on the error
        // row in the middle and press `>` (default step 1m): the
        // anchor falls forward to t=120, target is t=180, no event
        // satisfies → snap to max_top.  Then `<` from there should
        // anchor backward and land on row 0 (t=0 ≤ t=120-60=60).
        let events = vec![Some(ev_at(0, "a")), None, Some(ev_at(120, "b"))];
        let formatted = vec!["a".into(), "err".into(), "b".into()];
        let mut a = App::with_events(events, formatted);
        a.viewport_height = 5;
        a.active_tab_mut().viewport_top = LineIdx(1);
        a.handle_key(shift('>'));
        let max = a.active_tab().max_top(a.viewport_height, a.viewport_width);
        assert_eq!(a.active_tab().viewport_top, max);
        a.handle_key(shift('<'));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn step_keys_inside_dialog_dont_affect_step() {
        // While a dialog is open, `=`/`-`/`<`/`>` should be consumed
        // by the editor (typing them into the buffer) rather than
        // changing the step or scrolling.
        let mut a = app(10, 5);
        let before = a.time_step_idx;
        a.handle_key(key(KeyCode::Char('f')));
        a.handle_key(key(KeyCode::Char('=')));
        a.handle_key(key(KeyCode::Char('-')));
        a.handle_key(shift('>'));
        a.handle_key(shift('<'));
        assert_eq!(a.time_step_idx, before);
        assert_eq!(a.dialog.as_ref().unwrap().editor().unwrap().text, "=-><");
    }

    #[test]
    fn render_footer_shows_current_step() {
        let backend = TestBackend::new(120, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = App::with_rows(vec!["r".to_string()]);
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("step=1m"), "dump:\n{dump}");
        // Bumping the step is reflected on the next render.
        a.handle_key(key(KeyCode::Char('=')));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(dump.contains("step=5m"), "dump:\n{dump}");
    }

    // ---------- bookmarks ----------

    /// Drives `b` → j/k → Enter → typed name → Enter to create one
    /// named bookmark.  Uses [`select_app`] so `commit_selection` has
    /// real events whose positions and msgs can be captured.
    fn create_bookmark(a: &mut App, row: usize, name: Option<&str>) {
        a.handle_key(key(KeyCode::Char('b')));
        // Move the highlight from row 0 to `row`.
        for _ in 0..row {
            a.handle_key(key(KeyCode::Char('j')));
        }
        a.handle_key(key(KeyCode::Enter));
        // Bookmark-name dialog is open; type and confirm.
        if let Some(n) = name {
            for c in n.chars() {
                a.handle_key(key(KeyCode::Char(c)));
            }
        }
        a.handle_key(key(KeyCode::Enter));
    }

    #[test]
    fn b_enters_bookmark_mode_at_viewport_top() {
        let mut a = select_app(10, 5);
        a.active_tab_mut().viewport_top = LineIdx(3);
        a.handle_key(key(KeyCode::Char('b')));
        assert_eq!(a.active_tab().select, Some(bm_sel(3)));
    }

    #[test]
    fn b_is_noop_when_no_rows() {
        let mut a = App::with_rows(Vec::new());
        a.viewport_height = 5;
        a.handle_key(key(KeyCode::Char('b')));
        assert_eq!(a.active_tab().select, None);
    }

    #[test]
    fn bookmark_enter_opens_name_dialog() {
        let mut a = select_app(5, 5);
        a.handle_key(key(KeyCode::Char('b')));
        a.handle_key(key(KeyCode::Enter));
        assert!(matches!(a.dialog, Some(Dialog::BookmarkName { .. })));
        // Bookmark mode exited; selection cleared.
        assert!(a.active_tab().select.is_none());
    }

    #[test]
    fn bookmark_apply_with_blank_name_creates_unnamed() {
        let mut a = select_app(5, 5);
        create_bookmark(&mut a, 1, None);
        assert!(a.dialog.is_none());
        assert_eq!(a.session.bookmark_count(), 1);
        let bm = a.flat_bookmarks()[0];
        assert!(bm.name.is_none());
    }

    #[test]
    fn bookmark_apply_with_name_creates_named() {
        let mut a = select_app(5, 5);
        create_bookmark(&mut a, 2, Some("here"));
        assert_eq!(a.session.bookmark_count(), 1);
        let bm = a.flat_bookmarks()[0];
        assert_eq!(
            bm.name.as_ref().map(|n| n.to_string()),
            Some("here".into())
        );
    }

    #[test]
    fn bookmark_on_error_row_is_noop() {
        // An error row commits to neither the exclusion path nor the
        // bookmark path; mirror exclude-mode's silent-noop behavior.
        let events = vec![Some(ev("first")), None, Some(ev("third"))];
        let formatted = vec![
            "row 0".to_string(),
            "error: parse failure".to_string(),
            "row 2".to_string(),
        ];
        let mut a = App::with_events(events, formatted);
        a.viewport_height = 5;
        a.handle_key(key(KeyCode::Char('b')));
        a.handle_key(key(KeyCode::Char('j'))); // selection -> error row
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_none(), "no dialog should open for an error row");
        assert_eq!(a.session.bookmark_count(), 0);
        // Selection should remain intact so user can pick a different row.
        assert_eq!(a.active_tab().select, Some(bm_sel(1)));
    }

    #[test]
    fn bookmarks_tab_appears_iff_at_least_one_bookmark() {
        let mut a = select_app(5, 5);
        assert!(!a.has_bookmarks_tab());
        create_bookmark(&mut a, 0, None);
        assert!(a.has_bookmarks_tab());
    }

    #[test]
    fn bookmarks_tab_cycling_with_tab_key() {
        let mut a = select_app(5, 5);
        create_bookmark(&mut a, 0, None);
        // Now `tabs.len() == 1` plus the synthetic Bookmarks tab.
        assert_eq!(a.pane_count(), 2);
        // Active pane is still the regular tab (creating a bookmark
        // doesn't auto-switch).
        assert!(!a.bookmarks_active());
        a.handle_key(key(KeyCode::Tab));
        assert!(a.bookmarks_active());
        a.handle_key(key(KeyCode::Tab));
        assert!(!a.bookmarks_active());
    }

    #[test]
    fn bookmarks_tab_h_opens_help() {
        // The help popup should be reachable from the Bookmarks pane,
        // not just from regular tabs — `h` is the cross-pane help key.
        let mut a = select_app(5, 5);
        create_bookmark(&mut a, 0, None);
        a.handle_key(key(KeyCode::Tab));
        assert!(a.bookmarks_active());
        a.handle_key(key(KeyCode::Char('h')));
        assert!(matches!(a.dialog, Some(Dialog::Help { .. })));
    }

    #[test]
    fn bookmarks_tab_ignores_ctrl_w() {
        // Ctrl-W cannot close the synthetic Bookmarks tab.
        let mut a = select_app(5, 5);
        create_bookmark(&mut a, 0, None);
        a.handle_key(key(KeyCode::Tab));
        assert!(a.bookmarks_active());
        a.handle_key(ctrl('w'));
        // Still active; tabs.len() unchanged.
        assert!(a.bookmarks_active());
        assert_eq!(a.tabs.len(), 1);
    }

    #[test]
    fn bookmarks_tab_jk_moves_cursor() {
        let mut a = select_app(5, 5);
        create_bookmark(&mut a, 0, None);
        create_bookmark(&mut a, 1, None);
        create_bookmark(&mut a, 2, None);
        a.handle_key(key(KeyCode::Tab));
        assert!(a.bookmarks_active());
        // No cursor yet → first j initializes at row 0 then advances
        // to row 1.
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.bookmark_cursor_idx(), Some(1));
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.bookmark_cursor_idx(), Some(2));
        a.handle_key(key(KeyCode::Char('k')));
        assert_eq!(a.bookmark_cursor_idx(), Some(1));
    }

    #[test]
    fn bookmark_x_opens_confirmation_dialog() {
        let mut a = select_app(5, 5);
        create_bookmark(&mut a, 0, Some("named"));
        a.handle_key(key(KeyCode::Tab));
        assert!(a.bookmarks_active());
        // First j initializes the bookmark cursor at row 0; without it
        // `x` would no-op (no cursor → no row to delete).
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('x')));
        assert!(
            matches!(a.dialog, Some(Dialog::ConfirmDeleteBookmark { .. }),)
        );
        // Bookmark not yet deleted.
        assert_eq!(a.session.bookmark_count(), 1);
    }

    #[test]
    fn bookmark_x_confirm_deletes_and_hides_tab_when_last() {
        let mut a = select_app(5, 5);
        create_bookmark(&mut a, 0, None);
        a.handle_key(key(KeyCode::Tab));
        assert!(a.bookmarks_active());
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('x')));
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_none());
        assert_eq!(a.session.bookmark_count(), 0);
        // Synthetic tab gone; user is bounced back to the last regular tab.
        assert!(!a.has_bookmarks_tab());
        assert_eq!(a.active, 0);
    }

    #[test]
    fn bookmark_x_cancel_keeps_bookmark() {
        let mut a = select_app(5, 5);
        create_bookmark(&mut a, 0, None);
        a.handle_key(key(KeyCode::Tab));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('x')));
        a.handle_key(key(KeyCode::Esc));
        assert!(a.dialog.is_none());
        assert_eq!(a.session.bookmark_count(), 1);
    }

    #[test]
    fn session_persistence_round_trip_preserves_user_bookmarks() {
        // Build an App, create a couple of bookmarks, serialize the
        // resulting Session, deserialize, and confirm the bookmarks
        // (and the streams they reference) round-trip.  Doesn't touch
        // the filesystem — the actual `save_session`/`load_session`
        // wrappers are thin glue that's covered by test isolation
        // concerns we'd rather not introduce here.
        let mut a = select_app(5, 5);
        create_bookmark(&mut a, 0, Some("first"));
        create_bookmark(&mut a, 2, None);
        let json = serde_json::to_string(&a.session).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.version, seer::CURRENT_SESSION_VERSION);
        assert_eq!(restored.bookmark_count(), 2);
        // Both bookmarks are filed under the same stream id (the only
        // tab in this fixture).
        let stream_id = a.tabs[0].stream;
        let bms = restored.user_bookmarks.get(&stream_id).unwrap();
        assert_eq!(bms.len(), 2);
        let names: std::collections::HashSet<Option<String>> = bms
            .iter()
            .map(|b| b.name.as_ref().map(|n| n.to_string()))
            .collect();
        assert!(names.contains(&Some("first".to_string())));
        assert!(names.contains(&None));
        // The stream itself made the trip too — opening a fresh App
        // with the restored session would have access to its filter.
        assert!(restored.streams.get(&stream_id).is_some());
    }

    #[test]
    fn bookmark_enter_navigates_within_existing_tab() {
        // Make a bookmark on row 2, switch to Bookmarks tab, hit Enter.
        // The bookmark's stream is still open in tab 0, so we should
        // switch back to it.  We can't easily verify viewport_top
        // without a real engine (with_events synthesizes positions),
        // but we can verify the tab switch happens.
        let mut a = select_app(5, 5);
        create_bookmark(&mut a, 2, None);
        let original_tab = a.active;
        a.handle_key(key(KeyCode::Tab));
        assert!(a.bookmarks_active());
        // Initialize the bookmark cursor at the only entry.
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('k')));
        a.handle_key(key(KeyCode::Enter));
        assert!(!a.bookmarks_active());
        assert_eq!(a.active, original_tab);
    }

    #[test]
    fn bookmark_captures_nondefault_cursor_under_real_engine() {
        // With a real engine source attached, creating a bookmark on
        // a non-first record should capture a cursor whose offset is
        // non-zero — proof that the byte position of the bookmarked
        // event flowed through `commit_selection`'s
        // `streamview.cursor_before_record` call.
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[]),
            (20, "second", &[]),
            (30, "third", &[]),
        ]);
        // Without extras: each event is one display line.  Hide them
        // so row 2 == third event, regardless of the multi_line_app
        // default.
        a.handle_key(shift('F'));
        create_bookmark(&mut a, 2, Some("third"));
        let bm = a.flat_bookmarks()[0];
        // The bookmarked event sits well past byte 0.  The cursor's
        // entry for the source should match that position.
        let (_, offset) = bm.cursor.iter().next().expect("source recorded");
        assert!(
            offset.get() > 0,
            "expected non-zero offset, got {}",
            offset.get()
        );
        assert_eq!(bm.display_msg, "third");
        dir.cleanup();
    }

    /// Inserts a synthetic bookmark with a controllable `display_time`
    /// (`secs` epoch-seconds) and a user-given `label`.  Bypasses the
    /// `b`-key flow so tests can construct out-of-order timestamps
    /// directly — `create_bookmark` derives `display_time` from the
    /// underlying event, and `ev()` events all share a single fixed
    /// timestamp.
    fn add_bookmark_at(
        a: &mut App,
        stream_id: LogStreamId,
        secs: i64,
        label: &str,
    ) {
        let bm = Bookmark {
            id: BookmarkId::new_v4(),
            created_at: chrono::Utc::now(),
            cursor: Cursor::with([]),
            name: Some(BookmarkName::from(label.to_string())),
            display_source: SourceId::from("test".to_string()),
            display_time: chrono::TimeZone::timestamp_opt(
                &chrono::Utc,
                secs,
                0,
            )
            .single()
            .unwrap(),
            display_name: "Nexus".to_string(),
            display_msg: format!("msg @ {secs}"),
        };
        a.session.add_bookmark(stream_id, bm);
    }

    #[test]
    fn flat_bookmarks_sorted_by_display_time() {
        let mut a = select_app(5, 5);
        let stream_id = a.tabs[a.active].stream;
        // Insert in reverse-chronological order on `display_time`.
        // The returned order should still be ascending in `display_time`
        // regardless of insertion (and regardless of `created_at`,
        // which here advances monotonically with each call).
        add_bookmark_at(&mut a, stream_id, 300, "third");
        add_bookmark_at(&mut a, stream_id, 100, "first");
        add_bookmark_at(&mut a, stream_id, 200, "second");
        let names: Vec<String> = a
            .flat_bookmarks()
            .iter()
            .map(|b| b.name.as_ref().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn flat_bookmarks_ties_broken_by_created_at() {
        // Two bookmarks at the same display_time must still come back
        // in a stable, total order — the secondary key is created_at.
        let mut a = select_app(5, 5);
        let stream_id = a.tabs[a.active].stream;
        let t1 =
            chrono::TimeZone::timestamp_opt(&chrono::Utc, 1_700_000_000, 0)
                .single()
                .unwrap();
        let t2 =
            chrono::TimeZone::timestamp_opt(&chrono::Utc, 1_700_000_001, 0)
                .single()
                .unwrap();
        let same_display_time =
            chrono::TimeZone::timestamp_opt(&chrono::Utc, 500, 0)
                .single()
                .unwrap();
        a.session.add_bookmark(
            stream_id,
            Bookmark {
                id: BookmarkId::new_v4(),
                created_at: t2,
                cursor: Cursor::with([]),
                name: Some(BookmarkName::from("later".to_string())),
                display_source: SourceId::from("test".to_string()),
                display_time: same_display_time,
                display_name: "Nexus".to_string(),
                display_msg: "later-created".to_string(),
            },
        );
        a.session.add_bookmark(
            stream_id,
            Bookmark {
                id: BookmarkId::new_v4(),
                created_at: t1,
                cursor: Cursor::with([]),
                name: Some(BookmarkName::from("earlier".to_string())),
                display_source: SourceId::from("test".to_string()),
                display_time: same_display_time,
                display_name: "Nexus".to_string(),
                display_msg: "earlier-created".to_string(),
            },
        );
        let names: Vec<String> = a
            .flat_bookmarks()
            .iter()
            .map(|b| b.name.as_ref().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["earlier", "later"]);
    }

    #[test]
    fn bookmarks_pane_renders_three_columns() {
        // One bookmark rendered into a wide pane: every column's
        // content should be present in the dump.
        let mut a = select_app(5, 5);
        let stream_id = a.tabs[a.active].stream;
        add_bookmark_at(&mut a, stream_id, 100, "my-tag");
        a.handle_key(key(KeyCode::Tab));
        assert!(a.bookmarks_active());
        let backend = TestBackend::new(120, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        // Bookmarked event's timestamp (epoch 100 == 1970-01-01T00:01:40).
        assert!(
            dump.contains("1970-01-01T00:01:40.000Z"),
            "missing display_time; dump:\n{dump}",
        );
        assert!(dump.contains("my-tag"), "missing user name; dump:\n{dump}");
        assert!(
            dump.contains("Nexus: msg @ 100"),
            "missing app+msg; dump:\n{dump}",
        );
        assert!(
            dump.contains("bookmark created at"),
            "missing footer line; dump:\n{dump}",
        );
    }

    #[test]
    fn bookmarks_pane_wraps_long_message() {
        // A message far wider than the message column should wrap to
        // multiple lines without dropping content; the trailing
        // "bookmark created at" footer should still appear.
        let mut a = select_app(5, 5);
        let stream_id = a.tabs[a.active].stream;
        let long = "alpha beta gamma delta epsilon zeta \
                    eta theta iota kappa lambda";
        a.session.add_bookmark(
            stream_id,
            Bookmark {
                id: BookmarkId::new_v4(),
                created_at: chrono::Utc::now(),
                cursor: Cursor::with([]),
                name: None,
                display_source: SourceId::from("test".to_string()),
                display_time: chrono::TimeZone::timestamp_opt(
                    &chrono::Utc,
                    100,
                    0,
                )
                .single()
                .unwrap(),
                display_name: "App".to_string(),
                display_msg: long.to_string(),
            },
        );
        a.handle_key(key(KeyCode::Tab));
        // Narrow pane: msg column ends up around 17 cols, forcing the
        // long string to wrap into several rows.
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        // Every word in the message must appear somewhere in the dump
        // — wrapping must not drop content.
        for word in [
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta",
            "theta", "iota", "kappa", "lambda",
        ] {
            assert!(dump.contains(word), "missing word {word}; dump:\n{dump}",);
        }
        assert!(
            dump.contains("bookmark created at"),
            "missing footer; dump:\n{dump}",
        );
    }

    #[test]
    fn wrap_to_width_breaks_on_whitespace() {
        assert_eq!(
            wrap_to_width("hello world", 5),
            vec!["hello".to_string(), "world".to_string()],
        );
        assert_eq!(
            wrap_to_width("a b c d", 3),
            vec!["a b".to_string(), "c d".to_string()],
        );
    }

    #[test]
    fn wrap_to_width_breaks_oversized_word_by_column() {
        // A single word longer than the column width falls back to
        // column-boundary breaks via `column_chunks`.
        let r = wrap_to_width("supercalifragilistic", 5);
        assert_eq!(
            r,
            vec![
                "super".to_string(),
                "calif".to_string(),
                "ragil".to_string(),
                "istic".to_string(),
            ],
        );
    }

    #[test]
    fn wrap_to_width_edge_cases() {
        assert_eq!(wrap_to_width("", 10), vec!["".to_string()]);
        assert_eq!(wrap_to_width("hi", 0), vec!["".to_string()]);
    }

    #[test]
    fn bookmark_navigation_lands_on_bookmarked_event() {
        // Real-engine round-trip: bookmark the third event, scroll back
        // to the top, navigate to the bookmark, and verify the
        // streamview's window starts at the bookmarked record (so the
        // first formatted line carries its msg).
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[]),
            (20, "second", &[]),
            (30, "third", &[]),
        ]);
        a.handle_key(shift('F')); // hide extras: 3 events == 3 lines
        create_bookmark(&mut a, 2, Some("third"));
        // Scroll back to the top so the navigation has somewhere to go.
        a.handle_key(key(KeyCode::Char('g')));
        a.drain_long_op();
        assert_eq!(a.active_tab().viewport_top, 0);
        // Open Bookmarks tab and navigate.
        a.handle_key(key(KeyCode::Tab));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('k')));
        a.handle_key(key(KeyCode::Enter));
        a.drain_long_op();
        // After seek_to_cursor the window starts at the bookmarked
        // record, so its rendered line is at index 0 of the
        // materialized view.
        assert_eq!(a.active_tab().viewport_top, 0);
        let line0 = &a.active_tab().formatted()[0];
        assert!(line0.contains("third"), "first formatted line was {line0:?}");
        // No filter mismatch, so no notice.
        assert!(a.notice.is_none());
        dir.cleanup();
    }

    /// Returns the `msg` of the event currently at the top of the
    /// active tab's viewport, or `None` if the viewport is empty.
    /// Used by the apply-filter tests to verify which record landed
    /// at the top after a filter change.
    fn viewport_top_msg(a: &App) -> Option<String> {
        let tab = a.active_tab();
        let event_idx = *tab.event_for_line().get(tab.viewport_top.get())?;
        match tab.events().get(event_idx.get())? {
            Row::Event(ee) => Some(ee.event.msg.clone()),
            Row::Error(_) => None,
        }
    }

    /// Builds a 5-record multi-line app with a viewport short enough
    /// (2 lines) that `j`/`k` actually move the anchor instead of
    /// being clamped because everything already fits on screen.
    fn five_record_app() -> (App, TestDir) {
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[]),
            (20, "second", &[]),
            (30, "third", &[]),
            (40, "fourth", &[]),
            (50, "fifth", &[]),
        ]);
        a.viewport_height = 2;
        (a, dir)
    }

    #[test]
    fn apply_filter_preserves_viewport_when_anchor_survives() {
        // Scroll down to the third record, then apply a filter that
        // matches every record (effectively a no-op filter).  The
        // viewport should remain on the third record rather than
        // snapping back to the first.
        let (mut a, dir) = five_record_app();
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(viewport_top_msg(&a).as_deref(), Some("third"));
        let filter: Filter = "level>=trace".parse().unwrap();
        a.apply_filter(filter);
        a.drain_long_op();
        assert_eq!(viewport_top_msg(&a).as_deref(), Some("third"));
        dir.cleanup();
    }

    #[test]
    fn apply_filter_falls_forward_when_anchor_is_excluded() {
        // Scroll to the third record, then apply a filter that hides
        // exactly that record.  The viewport should slide to the next
        // visible record (the fourth) rather than snap to the top.
        let (mut a, dir) = five_record_app();
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(viewport_top_msg(&a).as_deref(), Some("third"));
        let filter: Filter = "msg!=third".parse().unwrap();
        a.apply_filter(filter);
        a.drain_long_op();
        assert_eq!(viewport_top_msg(&a).as_deref(), Some("fourth"));
        dir.cleanup();
    }

    #[test]
    fn apply_filter_falls_back_when_no_later_record_visible() {
        // Scroll the viewport down as far as it can go (with a
        // 2-line viewport on 5 records, that's the fourth), then
        // apply a filter that hides the fourth and fifth records.
        // With no visible record at or after the captured anchor,
        // the streamview falls back to the most recent visible
        // record before it (the third).  The 2-line viewport then
        // packs from the end of the surviving buffer — "second" at
        // top, "third" at the bottom — so the visible region is
        // full rather than half-empty.
        let (mut a, dir) = five_record_app();
        for _ in 0..3 {
            a.handle_key(key(KeyCode::Char('j')));
        }
        assert_eq!(viewport_top_msg(&a).as_deref(), Some("fourth"));
        let filter: Filter = "msg!=fourth msg!=fifth".parse().unwrap();
        a.apply_filter(filter);
        a.drain_long_op();
        let formatted = a.active_tab().formatted();
        assert!(
            formatted.iter().any(|l| l.contains("third")),
            "expected fallback to land 'third' in the viewport, \
             got {formatted:?}",
        );
        assert_eq!(viewport_top_msg(&a).as_deref(), Some("second"));
        dir.cleanup();
    }

    #[test]
    fn x_exclude_keeps_view_near_excluded_record() {
        // The exclude-mode commit also flows through `apply_filter`,
        // so it should preserve the viewport in the same way: the
        // `x`/`Enter` flow on the third record should leave the user
        // looking at the fourth, not at the first.
        let (mut a, dir) = five_record_app();
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('x')));
        // In exclude mode the selection cursor starts at viewport top
        // (the third record); Enter commits.
        a.handle_key(key(KeyCode::Enter));
        a.drain_long_op();
        assert_eq!(viewport_top_msg(&a).as_deref(), Some("fourth"));
        dir.cleanup();
    }

    #[test]
    fn bookmark_navigation_under_hiding_filter_sets_notice() {
        // After a filter hides the bookmarked event, navigating to the
        // bookmark should still work (anchor on the nearest visible
        // neighbor) and stash a notice telling the user.
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[]),
            (20, "second", &[]),
            (30, "third", &[]),
        ]);
        a.handle_key(shift('F'));
        create_bookmark(&mut a, 1, Some("second"));
        // Apply a filter that excludes the bookmarked event's msg.
        let filter: Filter = "msg!=second".parse().unwrap();
        a.apply_filter(filter);
        // Open Bookmarks tab and navigate.
        a.handle_key(key(KeyCode::Tab));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('k')));
        a.handle_key(key(KeyCode::Enter));
        let n = a.notice.as_deref().unwrap_or_default();
        assert!(n.contains("hidden"), "notice was: {n:?}");
        dir.cleanup();
    }

    // ---------- multi-line rendering ----------
    //
    // These tests exercise the line/event split that `render_rows` and
    // `Tab` use to support looker-style "header + indented extras"
    // output.  They build a real on-disk fixture (rather than going
    // through `App::with_events`) so the line→event mapping is built
    // by the production path.

    /// Writes one bunyan line per `(time_secs, msg, extras)` triple to a
    /// fresh temp file and returns the resulting App + path.  Each
    /// `extras` is a slice of `(key, json_value_string)` pairs joined
    /// into the JSON record verbatim — caller picks the right JSON
    /// shape for the value (e.g. `r#""0.1.0""#`, `42`).
    type RecordSpec<'a> = (i64, &'a str, &'a [(&'a str, &'a str)]);

    /// Builds an [`App`] over a synthetic bunyan file whose every
    /// record uses the supplied `hostname`.  Companion to
    /// [`multi_line_app`] for tests that need a non-trivial hostname
    /// (e.g. to exercise the H-toggle's short/full/none cycle).
    /// `records` is `(epoch_secs, msg)` pairs; extras aren't emitted
    /// because the hostname tests don't need them.
    fn host_app(hostname: &str, records: &[(i64, &str)]) -> (App, TestDir) {
        use std::io::Write;
        let dir = TestDir::new();
        let path = dir.path().join("a.log");
        for (secs, msg) in records {
            let time =
                chrono::DateTime::<chrono::Utc>::from_timestamp(*secs, 0)
                    .unwrap()
                    .to_rfc3339();
            let line = format!(
                r#"{{"v":0,"level":30,"name":"Nexus","hostname":{},"pid":1,"time":"{time}","msg":{}}}"#,
                serde_json::Value::String(hostname.to_string()),
                serde_json::Value::String(msg.to_string()),
            );
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(line.as_bytes())
                .unwrap();
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"\n")
                .unwrap();
        }
        let mut engine = Engine::new();
        engine.add_file_source(&path).unwrap();
        let mut a = App::new_for_tests(engine);
        a.viewport_height = 10;
        (a, dir)
    }

    fn multi_line_app(records: &[RecordSpec<'_>]) -> (App, TestDir) {
        use std::io::Write;
        let dir = TestDir::new();
        let path = dir.path().join("a.log");
        for (secs, msg, extras) in records {
            let time =
                chrono::DateTime::<chrono::Utc>::from_timestamp(*secs, 0)
                    .unwrap()
                    .to_rfc3339();
            let mut line = format!(
                r#"{{"v":0,"level":30,"name":"Nexus","hostname":"h","pid":1,"time":"{time}","msg":{}"#,
                serde_json::Value::String(msg.to_string()),
            );
            for (k, v) in *extras {
                line.push_str(&format!(",\"{k}\":{v}"));
            }
            line.push('}');
            line.push('\n');
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(line.as_bytes())
                .unwrap();
        }
        let mut engine = Engine::new();
        engine.add_file_source(&path).unwrap();
        let mut a = App::new_for_tests(engine);
        // The multi-line tests are about exactly the line/event split
        // that extras introduce.  Streams hide extras by default, so
        // flip the toggle on before handing the app back.
        a.toggle_show_extras();
        a.viewport_height = 10;
        (a, dir)
    }

    #[test]
    fn render_emits_multiple_lines_per_event_with_extras() {
        let (a, dir) = multi_line_app(&[
            (10, "starting", &[("build", r#""0.1.0""#)]),
            (20, "tick", &[]),
            (30, "loaded", &[("zones", "4"), ("ms", "12")]),
        ]);
        let tab = a.active_tab();
        // 3 events, but 6 display lines: 2 + 1 + 3.
        assert_eq!(tab.events().len(), 3);
        assert_eq!(tab.formatted().len(), 6);
        assert_eq!(
            tab.first_line_for_event(),
            &[LineIdx(0), LineIdx(2), LineIdx(3)],
        );
        assert_eq!(
            tab.event_for_line(),
            &[
                EventIdx(0),
                EventIdx(0),
                EventIdx(1),
                EventIdx(2),
                EventIdx(2),
                EventIdx(2),
            ],
        );
        // Spot-check the indented-extras layout.  msg sits at the end
        // of the header line, so the assertion latches onto its
        // trailing word.
        assert!(tab.formatted()[0].ends_with(" starting"));
        assert_eq!(tab.formatted()[1], r#"    build = "0.1.0""#);
        assert!(tab.formatted()[2].ends_with(" tick"));
        assert!(tab.formatted()[3].ends_with(" loaded"));
        assert_eq!(tab.formatted()[4], "    ms = 12");
        assert_eq!(tab.formatted()[5], "    zones = 4");
        dir.cleanup();
    }

    #[test]
    fn select_j_moves_to_next_event_skipping_extra_lines() {
        // First event has two extras (3 lines total); second has none
        // (1 line).  A single `j` in select mode must land on the
        // *second event*, not on one of the first event's extra rows.
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[("a", "1"), ("b", "2")]),
            (20, "second", &[]),
        ]);
        a.handle_key(key(KeyCode::Char('x')));
        assert_eq!(a.active_tab().select.unwrap().event_idx, 0);
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.active_tab().select.unwrap().event_idx, 1);
        dir.cleanup();
    }

    #[test]
    fn render_highlights_all_lines_of_selected_event() {
        // Event 1 spans lines 1, 2, 3 (header + 2 extras).  Selecting
        // it must paint the dark-gray bg on every one of those rows.
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[]),
            (20, "second", &[("a", "1"), ("b", "2")]),
            (30, "third", &[]),
        ]);
        a.handle_key(key(KeyCode::Char('x')));
        a.handle_key(key(KeyCode::Char('j'))); // selection -> event 1
        let backend = TestBackend::new(80, 7);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let buf = terminal.backend().buffer();
        // Layout: row 0 = tab bar, rows 1..6 = content (6 display
        // lines), row 6 = footer.  The selected event is at lines 1,
        // 2, 3 of the formatted vec → screen rows 2, 3, 4.
        for y in [2u16, 3, 4] {
            assert!(
                buf[(0, y)].style().bg == Some(Color::DarkGray),
                "expected DarkGray bg on line {y} of selected event",
            );
        }
        // The header above (event 0) and the line below (event 2)
        // must not be highlighted.
        assert!(buf[(0, 1)].style().bg != Some(Color::DarkGray));
        assert!(buf[(0, 5)].style().bg != Some(Color::DarkGray));
        dir.cleanup();
    }

    #[test]
    fn time_anchor_works_when_viewport_parks_on_extras_line() {
        // Park `viewport_top` on an extras row of event 0 (line 1) and
        // press `>` with the default 1-minute step.  The anchor must
        // resolve to event 0's timestamp (not crash with an out-of-
        // range index), and we should land on the first line of the
        // next event whose time ≥ anchor.time + 60s.  Tail events
        // ensure max_top is far enough below the target that the
        // result isn't accidentally clamped.
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[("k", "1")]), // lines 0, 1
            (80, "later", &[]),           // line 2 — 70s after first
            (90, "filler", &[]),          // line 3
            (100, "filler", &[]),         // line 4
            (110, "filler", &[]),         // line 5
        ]);
        a.viewport_height = 2;
        a.active_tab_mut().viewport_top = LineIdx(1); // an extras row of event 0
        a.handle_key(shift('>'));
        // step is 1m; from t=10 + 60s = 70 → next event at t=80 wins.
        // Its first display line is line 2; max_top with 6 lines and
        // height 2 is 4, so the result is not clamped.
        assert_eq!(a.active_tab().viewport_top, 2);
        dir.cleanup();
    }

    #[test]
    fn footer_reports_entry_count_in_select_mode() {
        // 3 events but 6 display lines: footer in select mode should
        // say "entry 1/3", not "row 1/6".
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[("k", "1")]),
            (20, "second", &[]),
            (30, "third", &[("k", "1"), ("z", "9")]),
        ]);
        a.handle_key(key(KeyCode::Char('x')));
        let backend = TestBackend::new(120, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(
            dump.contains("entry 1/3"),
            "expected entry count in footer, got:\n{dump}",
        );
        dir.cleanup();
    }

    // ---------- show_extras toggle (F) ----------

    #[test]
    fn streams_default_to_hiding_extras() {
        // A fresh stream produced by `push_tab` must hide structured
        // extras: the multi-line file below has two events, each with
        // its own extras, but the rendered tab should be just the two
        // header lines.
        use std::io::Write;
        let dir = TestDir::new();
        let path = dir.path().join("a.log");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"v":0,"level":30,"name":"N","hostname":"h","pid":1,"time":"2026-05-07T00:00:00Z","msg":"first","build":"0.1.0"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"v":0,"level":30,"name":"N","hostname":"h","pid":1,"time":"2026-05-07T00:00:01Z","msg":"second","zones":4}}"#
        )
        .unwrap();
        drop(f);
        let mut engine = Engine::new();
        engine.add_file_source(&path).unwrap();
        let a = App::new_for_tests(engine);
        let tab = a.active_tab();
        assert_eq!(tab.events().len(), 2);
        assert_eq!(tab.formatted().len(), 2, "extras should be hidden");
        assert!(tab.formatted()[0].ends_with(" first"));
        assert!(tab.formatted()[1].ends_with(" second"));
        assert!(!a.active_stream().show_extras);
        dir.cleanup();
    }

    #[test]
    fn shift_f_toggles_show_extras_and_repaints() {
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[("build", r#""0.1.0""#)]),
            (20, "tick", &[]),
        ]);
        // multi_line_app already enabled extras for its own tests; flip
        // it off, then back on, asserting the line count tracks the
        // setting and that `F` is the user-visible binding.
        assert!(a.active_stream().show_extras);
        assert_eq!(a.active_tab().formatted().len(), 3);
        a.handle_key(shift('F'));
        assert!(!a.active_stream().show_extras);
        assert_eq!(a.active_tab().formatted().len(), 2);
        // Bare `F` (some terminals don't set the SHIFT modifier) toggles
        // back on.
        a.handle_key(key(KeyCode::Char('F')));
        assert!(a.active_stream().show_extras);
        assert_eq!(a.active_tab().formatted().len(), 3);
        dir.cleanup();
    }

    #[test]
    fn show_extras_toggle_preserves_anchor_record() {
        // Three records, the second has two extras.  Park the viewport
        // on the second event's header (line 2 with extras showing) and
        // toggle off — viewport should snap to the same record at its
        // new (post-rerender) line.
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[]),
            (20, "second", &[("a", "1"), ("b", "2")]),
            (30, "third", &[]),
        ]);
        // With extras: lines = [first, second, a-row, b-row, third].
        a.active_tab_mut().viewport_top = LineIdx(1); // second event's header
        a.handle_key(shift('F')); // hide
        // Without extras: lines = [first, second, third].  The same
        // record's first line is now index 1.
        assert_eq!(a.active_tab().viewport_top, 1);
        assert_eq!(a.active_tab().formatted().len(), 3);
        a.handle_key(shift('F')); // show again
        // First line for record 1 is still index 1 (event 0 is single
        // line).  Anchor preserved across the second toggle too.
        assert_eq!(a.active_tab().viewport_top, 1);
        dir.cleanup();
    }

    // ---------- show_raw toggle (R) ----------

    #[test]
    fn shift_r_toggles_show_raw_and_renders_raw_bytes() {
        // With raw off, the formatted header carries the level/msg
        // columns.  Toggle R: the rendered row becomes the JSON line
        // from the source.  Toggle again: header returns.  Exercise
        // both NONE and SHIFT modifier forms of `R`.
        let (mut a, dir) =
            multi_line_app(&[(10, "first", &[("build", r#""0.1.0""#)])]);
        // multi_line_app enabled extras; the default (raw off) renders
        // a header followed by one extras row.
        assert!(!a.active_stream().show_raw);
        assert_eq!(a.active_tab().formatted().len(), 2);
        assert!(a.active_tab().formatted()[0].contains("INFO"));

        a.handle_key(shift('R'));
        assert!(a.active_stream().show_raw);
        // Raw mode is one line per record regardless of extras.
        assert_eq!(a.active_tab().formatted().len(), 1);
        assert!(
            a.active_tab().formatted()[0].starts_with('{')
                && a.active_tab().formatted()[0].contains(r#""msg":"first""#),
            "raw row should be the source JSON: {:?}",
            a.active_tab().formatted()[0],
        );

        // Bare `R` (terminals without the SHIFT modifier) flips back.
        a.handle_key(key(KeyCode::Char('R')));
        assert!(!a.active_stream().show_raw);
        assert_eq!(a.active_tab().formatted().len(), 2);
        assert!(a.active_tab().formatted()[0].contains("INFO"));
        dir.cleanup();
    }

    // ---------- show_date toggle (D) ----------

    #[test]
    fn streams_default_to_showing_date() {
        // A fresh stream's timestamps should include the date prefix:
        // most triage starts with "what day was this?" and the user
        // opts out (`D`) when they're zoomed in on a tight window.
        let (a, dir) = multi_line_app(&[(10, "first", &[])]);
        assert!(a.active_stream().show_date);
        let row = &a.active_tab().formatted()[0];
        // Timestamp prefix carries the date and ends in millisecond
        // precision with a `Z` suffix.
        assert!(
            row.starts_with("1970-01-01T00:00:10.000Z "),
            "expected dated header, got {row:?}",
        );
        dir.cleanup();
    }

    #[test]
    fn shift_d_toggles_show_date_and_repaints() {
        let (mut a, dir) = multi_line_app(&[(10, "first", &[])]);
        assert!(a.active_stream().show_date);
        let dated = a.active_tab().formatted()[0].clone();
        assert!(dated.starts_with("1970-01-01T00:00:10.000Z "));

        a.handle_key(shift('D'));
        assert!(!a.active_stream().show_date);
        let undated = a.active_tab().formatted()[0].clone();
        assert!(
            undated.starts_with("00:00:10.000Z "),
            "expected time-only header, got {undated:?}",
        );

        // Bare `D` (some terminals don't set SHIFT) toggles back on.
        a.handle_key(key(KeyCode::Char('D')));
        assert!(a.active_stream().show_date);
        assert!(a.active_tab().formatted()[0].starts_with("1970-01-01T"));
        dir.cleanup();
    }

    #[test]
    fn show_date_toggle_preserves_viewport_position() {
        // The line count per record is unchanged across a date toggle
        // (only the timestamp prefix shrinks), so a viewport parked
        // mid-buffer should stay where it is.  Contrast with the
        // show_extras toggle, which can collapse multi-line records.
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[]),
            (20, "second", &[]),
            (30, "third", &[]),
        ]);
        // Hide extras so the buffer is exactly one line per event;
        // that way `viewport_top` indexes records 1:1.
        a.handle_key(shift('F'));
        a.active_tab_mut().viewport_top = LineIdx(1);
        let lines_before = a.active_tab().formatted().len();
        a.handle_key(shift('D'));
        assert_eq!(a.active_tab().viewport_top, 1);
        assert_eq!(a.active_tab().formatted().len(), lines_before);
        dir.cleanup();
    }

    #[test]
    fn legacy_stream_without_show_date_defaults_to_true() {
        // Streams saved before `show_date` existed won't have the
        // field in their JSON.  Loading must default to `true` (the
        // new default), not `false` (bool::default).  This guards
        // against quietly dropping the date prefix on every
        // pre-existing project.
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "name": "Tab 1",
            "show_extras": false,
        });
        let stream: seer::LogStream = serde_json::from_value(json).unwrap();
        assert!(stream.show_date);
    }

    // ---------- field-display dialog (h) ----------

    /// Walks the open field-display dialog's cursor to `target` via
    /// repeated `j`.  Panics if the cursor doesn't reach the target —
    /// useful for diagnosing a missed navigation key in tests.
    fn focus_display_field(a: &mut App, target: DisplayFieldItem) {
        for _ in 0..DISPLAY_FIELD_ITEMS.len() * 2 {
            if let Some(Dialog::DisplayFields { cursor, .. }) = &a.dialog
                && DISPLAY_FIELD_ITEMS[*cursor] == target
            {
                return;
            }
            a.handle_key(key(KeyCode::Char('j')));
        }
        panic!("never landed on {target:?} after walking the dialog twice");
    }

    #[test]
    fn streams_default_to_short_hostname() {
        let (a, dir) = multi_line_app(&[(10, "first", &[])]);
        assert_eq!(a.active_stream().hostname_display, HostnameDisplay::Short);
        dir.cleanup();
    }

    #[test]
    fn d_opens_display_fields_dialog_with_active_opts() {
        let (mut a, dir) = multi_line_app(&[(10, "first", &[])]);
        a.handle_key(key(KeyCode::Char('d')));
        match &a.dialog {
            Some(Dialog::DisplayFields { draft, cursor }) => {
                assert_eq!(*cursor, 0);
                assert_eq!(*draft, a.active_stream().render_opts());
            }
            _ => panic!("expected DisplayFields dialog"),
        }
        dir.cleanup();
    }

    #[test]
    fn display_fields_dialog_jk_navigates_with_wrap() {
        let (mut a, dir) = multi_line_app(&[(10, "first", &[])]);
        a.handle_key(key(KeyCode::Char('d')));
        // j once → cursor 1.
        a.handle_key(key(KeyCode::Char('j')));
        match &a.dialog {
            Some(Dialog::DisplayFields { cursor, .. }) => {
                assert_eq!(*cursor, 1);
            }
            _ => panic!("dialog closed unexpectedly"),
        }
        // k once → cursor 0.
        a.handle_key(key(KeyCode::Char('k')));
        match &a.dialog {
            Some(Dialog::DisplayFields { cursor, .. }) => {
                assert_eq!(*cursor, 0);
            }
            _ => panic!("dialog closed unexpectedly"),
        }
        // k from 0 wraps to last.
        a.handle_key(key(KeyCode::Char('k')));
        match &a.dialog {
            Some(Dialog::DisplayFields { cursor, .. }) => {
                assert_eq!(*cursor, DISPLAY_FIELD_ITEMS.len() - 1);
            }
            _ => panic!("dialog closed unexpectedly"),
        }
        dir.cleanup();
    }

    #[test]
    fn display_fields_dialog_tab_navigates_like_j() {
        // Tab should advance the cursor the same way `j` does, and
        // BackTab should retreat it like `k` — for users who'd rather
        // use Tab/Shift-Tab than vim keys.
        let (mut a, dir) = multi_line_app(&[(10, "first", &[])]);
        a.handle_key(key(KeyCode::Char('d')));
        a.handle_key(key(KeyCode::Tab));
        match &a.dialog {
            Some(Dialog::DisplayFields { cursor, .. }) => {
                assert_eq!(*cursor, 1);
            }
            _ => panic!("dialog closed unexpectedly"),
        }
        a.handle_key(back_tab());
        match &a.dialog {
            Some(Dialog::DisplayFields { cursor, .. }) => {
                assert_eq!(*cursor, 0);
            }
            _ => panic!("dialog closed unexpectedly"),
        }
        dir.cleanup();
    }

    #[test]
    fn display_fields_dialog_space_selects_hostname_radio() {
        // Walk the cursor to `HostnameFull`, hit space — the draft
        // should reflect Full, but the active stream is unchanged
        // until Enter.  Then Enter applies and the stream picks up
        // Full.
        let (mut a, dir) = multi_line_app(&[(10, "first", &[])]);
        a.handle_key(key(KeyCode::Char('d')));
        focus_display_field(&mut a, DisplayFieldItem::HostnameFull);
        a.handle_key(key(KeyCode::Char(' ')));
        match &a.dialog {
            Some(Dialog::DisplayFields { draft, .. }) => {
                assert_eq!(draft.hostname, HostnameDisplay::Full);
            }
            _ => panic!("dialog closed unexpectedly"),
        }
        // Active stream still on Short — draft hasn't been applied.
        assert_eq!(a.active_stream().hostname_display, HostnameDisplay::Short);
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_none());
        assert_eq!(a.active_stream().hostname_display, HostnameDisplay::Full);
        dir.cleanup();
    }

    #[test]
    fn display_fields_dialog_space_toggles_pid_checkbox() {
        let (mut a, dir) = multi_line_app(&[(10, "first", &[])]);
        let before = a.active_stream().render_opts();
        assert!(!before.show_pid, "default is pid hidden");
        a.handle_key(key(KeyCode::Char('d')));
        focus_display_field(&mut a, DisplayFieldItem::Pid);
        a.handle_key(key(KeyCode::Char(' ')));
        a.handle_key(key(KeyCode::Enter));
        assert!(a.active_stream().render_opts().show_pid);
        dir.cleanup();
    }

    #[test]
    fn display_fields_dialog_esc_discards_draft() {
        let (mut a, dir) = multi_line_app(&[(10, "first", &[])]);
        let before = a.active_stream().render_opts();
        a.handle_key(key(KeyCode::Char('d')));
        focus_display_field(&mut a, DisplayFieldItem::HostnameNone);
        a.handle_key(key(KeyCode::Char(' ')));
        a.handle_key(key(KeyCode::Esc));
        assert!(a.dialog.is_none());
        assert_eq!(a.active_stream().render_opts(), before);
        dir.cleanup();
    }

    #[test]
    fn display_fields_dialog_repaints_on_apply() {
        // Use a hostname that exercises both the dot-trim and the
        // UUID-collapse, so the two hostname modes produce visibly
        // different rendered lines.
        let (mut a, dir) = host_app(
            "oxz_nexus_c53300fc-84eb-490a-9e1e-9e18d372856d.oxide.test",
            &[(10, "first")],
        );
        let short_line = a.active_tab().formatted()[0].clone();
        assert!(
            short_line.contains(" oxz_nexus_c53300fc Nexus "),
            "expected short hostname (and no pid by default), got \
             {short_line:?}",
        );
        // d → focus HostnameFull → space → Enter.
        a.handle_key(key(KeyCode::Char('d')));
        focus_display_field(&mut a, DisplayFieldItem::HostnameFull);
        a.handle_key(key(KeyCode::Char(' ')));
        a.handle_key(key(KeyCode::Enter));
        let full_line = a.active_tab().formatted()[0].clone();
        assert!(
            full_line.contains(
                " oxz_nexus_c53300fc-84eb-490a-9e1e-9e18d372856d.oxide.test \
                 Nexus ",
            ),
            "expected full hostname after apply, got {full_line:?}",
        );
        dir.cleanup();
    }

    #[test]
    fn all_render_opts_persist_into_session_round_trip() {
        // Flip every `RenderOpts` dimension to a non-default value
        // and confirm the whole set rides through a serde round-trip.
        // Item 3's destructure-on-copy in `LogStream::render_opts` /
        // `set_render_opts` is what enforces propagation when a new
        // field lands; this test confirms the persistence layer
        // preserves whatever the stream is carrying.
        let (mut a, dir) = multi_line_app(&[(10, "first", &[("k", "1")])]);
        let stream_id = a.tabs[a.active].stream;
        let initial = a.active_stream();
        assert!(initial.show_extras, "multi_line_app starts with extras on");
        assert!(!initial.show_raw);
        assert!(initial.show_date);
        assert!(!initial.show_pid);
        assert!(initial.show_name);
        assert_eq!(initial.hostname_display, HostnameDisplay::Short);

        // Keybinding toggles for the three with dedicated shortcuts.
        a.handle_key(shift('F')); // extras off
        a.handle_key(shift('R')); // raw on
        a.handle_key(shift('D')); // date off
        // Dialog handles the rest: pid on, name off, hostname Full.
        a.handle_key(key(KeyCode::Char('d')));
        focus_display_field(&mut a, DisplayFieldItem::Pid);
        a.handle_key(key(KeyCode::Char(' ')));
        focus_display_field(&mut a, DisplayFieldItem::Name);
        a.handle_key(key(KeyCode::Char(' ')));
        focus_display_field(&mut a, DisplayFieldItem::HostnameFull);
        a.handle_key(key(KeyCode::Char(' ')));
        a.handle_key(key(KeyCode::Enter));

        let after = a.active_stream();
        assert!(!after.show_extras);
        assert!(after.show_raw);
        assert!(!after.show_date);
        assert!(after.show_pid);
        assert!(!after.show_name);
        assert_eq!(after.hostname_display, HostnameDisplay::Full);

        let json = serde_json::to_string(&a.session).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        let stream = restored.streams.get(&stream_id).unwrap();
        assert!(!stream.show_extras);
        assert!(stream.show_raw);
        assert!(!stream.show_date);
        assert!(stream.show_pid);
        assert!(!stream.show_name);
        assert_eq!(stream.hostname_display, HostnameDisplay::Full);
        dir.cleanup();
    }

    #[test]
    fn legacy_stream_without_hostname_display_defaults_to_short() {
        // `hostname_display` was added after `show_extras`/`show_date`;
        // legacy stream JSON won't carry it.  Loading must default to
        // `Short` (the new default) rather than crashing or silently
        // falling back to a different variant.
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "name": "Tab 1",
            "show_extras": false,
        });
        let stream: seer::LogStream = serde_json::from_value(json).unwrap();
        assert_eq!(stream.hostname_display, HostnameDisplay::Short);
    }

    // ---------- Summary tab ----------

    #[test]
    fn shift_s_opens_summary_tab_without_dialog() {
        // `S` should mint a fresh tab of kind Summary and switch to it
        // without prompting for a filter — the new tab inherits the
        // active tab's filter and the user adjusts it afterwards via
        // `f` if they want to.
        let (mut a, dir) =
            multi_line_app(&[(10, "first", &[]), (20, "second", &[])]);
        let initial_tabs = a.tabs.len();
        a.handle_key(shift('S'));
        assert_eq!(a.tabs.len(), initial_tabs + 1);
        assert_eq!(a.active, a.tabs.len() - 1);
        assert_eq!(a.active_tab().kind, TabKind::Summary);
        assert!(a.dialog.is_none());
        dir.cleanup();
    }

    #[test]
    fn shift_s_inherits_active_filter() {
        // Set a non-default filter on the current tab; the new
        // Summary tab should pick that up rather than default.  We
        // verify by making the filter accept zero events and then
        // observing that the summary reports zero events.
        let (mut a, dir) =
            multi_line_app(&[(10, "alpha", &[]), (20, "beta", &[])]);
        let f: Filter = "msg=alpha".parse().unwrap();
        a.apply_filter(f.clone());
        a.handle_key(shift('S'));
        a.drain_long_op();
        assert_eq!(a.active_tab().kind, TabKind::Summary);
        // Inherited filter should leave only the one matching event;
        // the summary's first line records the count.
        assert!(
            a.active_tab().formatted()[0].starts_with("Summary: 1 event"),
            "summary should reflect inherited filter; got {:?}",
            a.active_tab().formatted().first(),
        );
        // And the underlying stream's filter is the same the user had.
        let stream_id = a.active_tab().stream;
        let stream_filter = &a.session.streams.get(&stream_id).unwrap().filter;
        assert_eq!(stream_filter.to_string(), f.to_string());
        dir.cleanup();
    }

    #[test]
    fn summary_tab_f_opens_filter_dialog() {
        // After landing on a Summary tab the user can still adjust
        // the filter via `f`.
        let (mut a, dir) = multi_line_app(&[(10, "first", &[])]);
        a.handle_key(shift('S'));
        assert!(a.dialog.is_none());
        a.handle_key(key(KeyCode::Char('f')));
        assert!(matches!(a.dialog, Some(Dialog::Filter { .. })));
        dir.cleanup();
    }

    #[test]
    fn bare_s_opens_summary_tab() {
        // Some terminals report `S` with no SHIFT modifier; the binding
        // accepts both forms so capital-S is reliable across them.
        let (mut a, dir) = multi_line_app(&[(10, "first", &[])]);
        a.handle_key(key(KeyCode::Char('S')));
        assert_eq!(a.active_tab().kind, TabKind::Summary);
        dir.cleanup();
    }

    #[test]
    fn summary_tab_renders_field_and_time_sections() {
        // Open a summary tab over a multi-record file; the rendered
        // formatted lines should include the standard section headers
        // ("Summary:", "== name ...", "== time ...").
        let (mut a, dir) = multi_line_app(&[
            (10, "first", &[]),
            (20, "first", &[]),
            (30, "second", &[]),
        ]);
        a.handle_key(shift('S'));
        a.drain_long_op();
        assert!(a.dialog.is_none());
        assert_eq!(a.active_tab().kind, TabKind::Summary);
        let lines = a.active_tab().formatted();
        assert!(lines.iter().any(|l| l.starts_with("Summary: 3 events")));
        assert!(lines.iter().any(|l| l.starts_with("== name")));
        assert!(lines.iter().any(|l| l.starts_with("== msg")));
        assert!(lines.iter().any(|l| l.starts_with("== time")));
        dir.cleanup();
    }

    #[test]
    fn summary_tab_filter_apply_re_renders() {
        // After landing on a Summary tab, the user can open the
        // filter dialog with `f` and apply a narrower filter; the
        // histogram should re-render against the new filter.
        let (mut a, dir) =
            multi_line_app(&[(10, "first", &[]), (20, "second", &[])]);
        a.handle_key(shift('S'));
        a.drain_long_op();
        // Open the filter dialog with `f`, type a narrowing filter,
        // and apply.
        a.handle_key(key(KeyCode::Char('f')));
        let d = a.dialog.as_mut().unwrap();
        type_into(d, "msg=second");
        a.handle_key(key(KeyCode::Enter));
        a.drain_long_op();
        assert!(a.dialog.is_none());
        let lines = a.active_tab().formatted();
        assert!(
            lines.iter().any(|l| l.starts_with("Summary: 1 event")),
            "expected one-event summary, got:\n{}",
            lines.join("\n"),
        );
        dir.cleanup();
    }

    #[test]
    fn summary_tab_keeps_select_mode_inactive() {
        // x/X/b are no-ops on Summary tabs because there are no
        // underlying records to act on.  A Summary tab whose key
        // ignores the binding shouldn't suddenly drop into selection
        // mode and trap the user.
        let (mut a, dir) = multi_line_app(&[(10, "first", &[])]);
        a.handle_key(shift('S'));
        a.handle_key(key(KeyCode::Char('x')));
        assert!(a.active_tab().select.is_none());
        a.handle_key(shift('X'));
        assert!(a.active_tab().select.is_none());
        a.handle_key(key(KeyCode::Char('b')));
        assert!(a.active_tab().select.is_none());
        dir.cleanup();
    }

    #[test]
    fn summary_tab_footer_omits_record_only_keys() {
        // Summary tabs hide x/X/b/F/<>/= from the footer because those
        // bindings either no-op (selection-mode) or operate on event
        // state the summary view doesn't expose.
        let (mut a, dir) = multi_line_app(&[(10, "first", &[])]);
        a.handle_key(shift('S'));
        let backend = TestBackend::new(160, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        // Summary footer should still mention quit and help — the
        // always-on chips that every footer carries.
        assert!(dump.contains("q quit"), "dump:\n{dump}");
        assert!(dump.contains("h help"), "dump:\n{dump}");
        // ...but not the record-oriented bindings.
        assert!(!dump.contains("x/X exclude"), "dump:\n{dump}");
        assert!(!dump.contains("F fields="), "dump:\n{dump}");
        dir.cleanup();
    }

    #[test]
    fn summary_tab_tab_name_is_summary_n() {
        let (mut a, dir) = multi_line_app(&[(10, "first", &[])]);
        a.handle_key(shift('S'));
        assert!(
            a.active_tab().name.starts_with("Summary "),
            "expected `Summary N`, got {:?}",
            a.active_tab().name,
        );
        dir.cleanup();
    }

    #[test]
    fn summary_tab_renders_after_build_completes() {
        // Regression: after a Summary build finalizes, the tab's
        // `event_for_line` is empty (Summary tabs have no underlying
        // events, only histogram rows).  `format_user_status` used to
        // index into that empty slice and panic on the next frame.
        let (mut a, dir) =
            multi_line_app(&[(10, "first", &[]), (20, "second", &[])]);
        a.handle_key(shift('S'));
        a.drain_long_op();
        let backend = TestBackend::new(160, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        dir.cleanup();
    }

    // ---------- persistent-session plumbing (phase 5) ----------

    #[test]
    fn build_session_sources_canonicalizes_and_stats_each_file() {
        let dir = TestDir::new();
        let a_path = dir.path().join("a.log");
        let b_path = dir.path().join("b.log");
        std::fs::write(&a_path, b"hello\n").unwrap();
        std::fs::write(&b_path, b"").unwrap();

        let mut engine = Engine::new();
        let sources = build_session_sources(
            &[a_path.clone(), b_path.clone()],
            &mut engine,
        )
        .unwrap();

        assert_eq!(sources.len(), 2);
        // Order is preserved (`IdOrdMap` iterates by id, which for
        // file sources is the canonical path, so the two test files
        // come out in the same lexical order they went in).
        let by_id: Vec<&SessionSource> = sources.iter().collect();
        assert_eq!(by_id[0].path, a_path.canonicalize_utf8().unwrap());
        assert_eq!(by_id[1].path, b_path.canonicalize_utf8().unwrap());
        // Sizes match the bytes we wrote.
        assert_eq!(by_id[0].size, 6);
        assert_eq!(by_id[1].size, 0);
        // The SourceId the engine assigned matches the one recorded
        // in the SessionSource — that's the alignment cursors and
        // bookmarks rely on.
        let id_as_str: &str = by_id[0].id.as_ref();
        assert_eq!(id_as_str, by_id[0].path.as_str());
        dir.cleanup();
    }

    #[test]
    fn try_save_now_persists_session_and_marks_policy_clean() {
        let dir = TestDir::new();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        let session_id = Session::new().id;

        let mut a = App::new_for_tests(Engine::new());
        a.session.id = session_id;
        a.store = Some(store);
        // Simulate a debounced mutation having dirtied the session.
        a.policy.record(seer::Cadence::Debounced);
        assert!(a.policy.dirty());

        a.try_save_now().unwrap();
        assert!(!a.policy.dirty());

        // Round-trip: the file the store wrote should load back to the
        // same session id.
        let reloaded = a.store.as_ref().unwrap().load(session_id).unwrap();
        assert_eq!(reloaded.id, session_id);
        dir.cleanup();
    }

    #[test]
    fn try_save_now_is_a_noop_without_a_store() {
        // A transient session (phase 8) has `store: None`; calling
        // try_save_now must succeed without touching disk.  The
        // policy's dirty bit is also left alone — there's no flush
        // to record.
        let mut a = App::new_for_tests(Engine::new());
        assert!(a.store.is_none());
        a.policy.record(seer::Cadence::Debounced);
        a.try_save_now().expect("no-op should succeed");
        assert!(a.policy.dirty(), "dirty bit untouched without a store");
    }

    // ---------- inline persistence (phase 6) ----------

    /// Builds an [`App`] with no engine but with a freshly-created
    /// on-disk [`SessionStore`] attached.  Returns the [`TestDir`]
    /// so the caller can keep the backing directory alive for the
    /// duration of the test (and call [`TestDir::cleanup`] on
    /// success).
    fn app_with_store_and_one_tab() -> (App, TestDir) {
        let dir = TestDir::new();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        let mut a = App::new_for_tests(Engine::new());
        a.store = Some(store);
        // The default `App::new` already pushed a tab; that push
        // would itself have saved if a store had been attached at
        // construction time.  Save now so the disk file reflects the
        // app's current state.
        a.save_after_inline_mutation();
        (a, dir)
    }

    /// Reloads the [`App`]'s session from disk through its attached
    /// store.  Panics if the store is `None` or the file can't be
    /// loaded.
    fn reload_session(a: &App) -> Session {
        let id = a.session.id;
        a.store.as_ref().unwrap().load(id).unwrap()
    }

    #[test]
    fn add_bookmark_persists_inline() {
        let (mut a, dir) = app_with_store_and_one_tab();
        assert!(reload_session(&a).user_bookmarks.is_empty());

        let stream_id = a.tabs[a.active].stream;
        let draft = BookmarkDraft {
            cursor: Cursor::with([]),
            display_source: SourceId::from("test".to_string()),
            display_time: chrono::Utc::now(),
            display_name: "Nexus".to_string(),
            display_msg: "marked".to_string(),
        };
        a.add_bookmark(Some(BookmarkName::from("here".to_string())), draft);

        assert!(!a.policy.dirty(), "save flushed the dirty bit");
        let reloaded = reload_session(&a);
        let bms = reloaded.user_bookmarks.get(&stream_id).unwrap();
        assert_eq!(bms.len(), 1);
        let only = bms.iter().next().unwrap();
        assert_eq!(only.display_msg, "marked");
        dir.cleanup();
    }

    #[test]
    fn delete_bookmark_persists_inline() {
        let (mut a, dir) = app_with_store_and_one_tab();
        let stream_id = a.tabs[a.active].stream;
        let draft = BookmarkDraft {
            cursor: Cursor::with([]),
            display_source: SourceId::from("test".to_string()),
            display_time: chrono::Utc::now(),
            display_name: "Nexus".to_string(),
            display_msg: "doomed".to_string(),
        };
        a.add_bookmark(None, draft);
        let id = a.session.user_bookmarks[&stream_id].iter().next().unwrap().id;

        a.delete_bookmark(id);
        assert!(!a.policy.dirty());
        let reloaded = reload_session(&a);
        assert!(reloaded.user_bookmarks.is_empty());
        dir.cleanup();
    }

    #[test]
    fn push_tab_persists_the_new_stream_inline() {
        let (mut a, dir) = app_with_store_and_one_tab();
        let before = reload_session(&a).streams.len();

        a.push_tab(TabKind::Stream, Filter::default());
        assert!(!a.policy.dirty());
        let after = reload_session(&a).streams.len();
        assert_eq!(after, before + 1);
        dir.cleanup();
    }

    #[test]
    fn save_mirrors_tabs_into_session_tabs() {
        // `app_with_store_and_one_tab` already saved once with the
        // default single tab.  Push a second tab, then read back the
        // session to confirm both tabs were serialized in order.
        let (mut a, dir) = app_with_store_and_one_tab();
        let first_stream = a.tabs[0].stream;
        a.push_tab(TabKind::Stream, Filter::default());
        let second_stream = a.tabs[1].stream;

        let reloaded = reload_session(&a);
        assert_eq!(reloaded.tabs.len(), 2);
        assert_eq!(reloaded.tabs[0].stream, first_stream);
        assert_eq!(reloaded.tabs[0].kind, TabKind::Stream);
        assert_eq!(reloaded.tabs[1].stream, second_stream);
        assert_eq!(reloaded.tabs[1].kind, TabKind::Stream);
        dir.cleanup();
    }

    #[test]
    fn rename_persists_inline_and_survives_reload() {
        // Renaming the active tab via the dialog should flush to disk
        // immediately and the new name should come back on resume —
        // pinning the bug where renames lived only on the runtime
        // `Tab` and were lost the moment the session was reloaded.
        let (mut a, dir) = app_with_store_and_one_tab();

        a.handle_key(key(KeyCode::Char('r')));
        a.handle_key(ctrl('u'));
        type_into(a.dialog.as_mut().unwrap(), "Nexus");
        a.handle_key(key(KeyCode::Enter));

        assert!(!a.policy.dirty(), "save flushed the dirty bit");
        let reloaded = reload_session(&a);
        assert_eq!(reloaded.tabs.len(), 1);
        assert_eq!(reloaded.tabs[0].name, "Nexus");

        // Resume on the reloaded session should hand the App back a tab
        // with the renamed name, not the original "Tab 1".
        let app2 = App::new_with_session(
            Engine::new(),
            reloaded,
            None,
            SavePolicy::new(SavePolicy::DEFAULT_DEBOUNCE),
        );
        assert_eq!(app2.tabs.len(), 1);
        assert_eq!(app2.tabs[0].name, "Nexus");
        dir.cleanup();
    }

    #[test]
    fn save_records_summary_tab_kind() {
        // A Summary tab persists with `kind: Summary` so resume can
        // restore the histogram view rather than a stream view.
        let (mut a, dir) = app_with_store_and_one_tab();
        a.push_tab(TabKind::Summary, Filter::default());

        let reloaded = reload_session(&a);
        let summary = reloaded
            .tabs
            .iter()
            .find(|t| t.kind == TabKind::Summary)
            .expect("the summary tab should round-trip");
        assert_eq!(summary.stream, a.tabs[1].stream);
        dir.cleanup();
    }

    #[test]
    fn resume_restores_persisted_tabs_into_app() {
        // Build a session with two persisted tabs and feed it through
        // `App::new_with_session`: the resulting App should have those
        // two runtime tabs in the same order, both backed by the
        // streams the session carries — not the legacy single fresh
        // tab the pre-restore code path produced.
        let mut session = Session::new();
        let stream_a = LogStream::new("Tab 7".to_string());
        let stream_b = LogStream::new("Summary 9".to_string());
        let id_a = stream_a.id;
        let id_b = stream_b.id;
        session.streams.insert_unique(stream_a).unwrap();
        session.streams.insert_unique(stream_b).unwrap();
        session.tabs.push(seer::Tab {
            name: "Tab 7".to_string(),
            stream: id_a,
            kind: TabKind::Stream,
            cursor: None,
        });
        session.tabs.push(seer::Tab {
            name: "Summary 9".to_string(),
            stream: id_b,
            kind: TabKind::Summary,
            cursor: None,
        });

        let app = App::new_with_session(
            Engine::new(),
            session,
            None,
            SavePolicy::new(SavePolicy::DEFAULT_DEBOUNCE),
        );

        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.tabs[0].stream, id_a);
        assert_eq!(app.tabs[0].kind, TabKind::Stream);
        assert_eq!(app.tabs[1].stream, id_b);
        assert_eq!(app.tabs[1].kind, TabKind::Summary);
        // `next_tab_number` should be past every restored "Tab N" /
        // "Summary N" so a newly-pushed tab doesn't collide.
        assert!(app.next_tab_number > 9);
    }

    #[test]
    fn resume_with_no_persisted_tabs_falls_back_to_a_fresh_tab() {
        // A session with no tabs (the legacy shape, before tab
        // persistence) must still produce a viable App: one fresh
        // Stream tab so the "tabs is never empty" invariant holds.
        let session = Session::new();
        let app = App::new_with_session(
            Engine::new(),
            session,
            None,
            SavePolicy::new(SavePolicy::DEFAULT_DEBOUNCE),
        );
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].kind, TabKind::Stream);
    }

    #[test]
    fn resume_drops_tabs_pointing_at_missing_streams() {
        // A persisted tab whose stream is gone (the user removed it,
        // or the session got truncated somehow) gets quietly skipped
        // rather than panicking.  When every persisted tab is broken,
        // the App falls back to the fresh-tab default.
        let mut session = Session::new();
        let phantom = LogStream::new("Tab 1".to_string()).id;
        session.tabs.push(seer::Tab {
            name: "Tab 1".to_string(),
            stream: phantom,
            kind: TabKind::Stream,
            cursor: None,
        });

        let app = App::new_with_session(
            Engine::new(),
            session,
            None,
            SavePolicy::new(SavePolicy::DEFAULT_DEBOUNCE),
        );
        assert_eq!(app.tabs.len(), 1);
        // The fallback tab is a *new* stream, not the missing one.
        assert_ne!(app.tabs[0].stream, phantom);
    }

    #[test]
    fn apply_filter_persists_the_new_filter_inline() {
        let (mut a, dir) = app_with_store_and_one_tab();
        let stream_id = a.tabs[a.active].stream;
        let mut new_filter = Filter::default();
        new_filter.add_predicate(
            EventPredicate::MsgMatches {
                regex: regex::Regex::new("hello").unwrap(),
                form: Form::Affirmed,
            }
            .into(),
        );

        a.apply_filter(new_filter.clone());
        assert!(!a.policy.dirty());
        let reloaded = reload_session(&a);
        let stream = reloaded.streams.get(&stream_id).unwrap();
        // Filter doesn't implement Eq, so compare its display form —
        // round-trip via serde is the contract that matters anyway.
        assert_eq!(format!("{:?}", stream.filter), format!("{new_filter:?}"),);
        dir.cleanup();
    }

    #[test]
    fn toggle_show_extras_persists_inline() {
        let (mut a, dir) = app_with_store_and_one_tab();
        let stream_id = a.tabs[a.active].stream;
        let before =
            reload_session(&a).streams.get(&stream_id).unwrap().show_extras;

        a.toggle_show_extras();
        assert!(!a.policy.dirty());
        let after =
            reload_session(&a).streams.get(&stream_id).unwrap().show_extras;
        assert_eq!(after, !before);
        dir.cleanup();
    }

    // ---------- CLI surface (phase 9) ----------

    #[test]
    fn format_session_list_empty_returns_friendly_message() {
        let out = format_session_list(&[]);
        assert_eq!(out, "(no saved sessions)\n");
    }

    #[test]
    fn format_session_list_contains_one_row_per_session() {
        let s1 = Session::new();
        let s2 = Session::new();
        let out = format_session_list(&[s1.clone(), s2.clone()]);
        // Header line + 2 data lines = 3 newlines total.
        assert_eq!(out.matches('\n').count(), 3);
        // The ids should appear in the output.
        assert!(out.contains(&s1.id.to_string()));
        assert!(out.contains(&s2.id.to_string()));
        // The header line is present.
        assert!(out.lines().next().unwrap().contains("LAST SAVED"));
    }

    #[test]
    fn truncate_path_head_keeps_short_strings_intact() {
        assert_eq!(truncate_path_head("/foo/bar", 60), "/foo/bar");
    }

    #[test]
    fn truncate_path_head_chops_the_front_with_an_ellipsis() {
        let long = "/a/very/very/long/path/to/some/file.log";
        let out = truncate_path_head(long, 20);
        assert_eq!(out.chars().count(), 20);
        assert!(out.starts_with("..."));
        // The trailing filename is preserved.
        assert!(out.ends_with("file.log"));
    }

    #[test]
    fn engine_for_resumed_session_errors_naming_missing_paths() {
        let dir = TestDir::new();
        let exists = dir.path().join("present.log");
        std::fs::write(&exists, b"hi\n").unwrap();
        let missing = dir.path().join("gone.log");
        // Do *not* create `missing`.

        let mut s = Session::new();
        s.sources
            .insert_unique(SessionSource {
                id: SourceId::from(exists.as_str().to_string()),
                path: exists.clone(),
                mtime: chrono::Utc::now(),
                size: 3,
            })
            .unwrap();
        s.sources
            .insert_unique(SessionSource {
                id: SourceId::from(missing.as_str().to_string()),
                path: missing.clone(),
                mtime: chrono::Utc::now(),
                size: 0,
            })
            .unwrap();
        let Err(err) = engine_for_resumed_session(&s) else {
            panic!("expected error when a source file is missing");
        };
        let msg = err.to_string();
        assert!(msg.contains("missing source files"), "msg = {msg}");
        assert!(
            msg.contains(missing.as_str()),
            "expected msg to name {missing}, got {msg}"
        );
        dir.cleanup();
    }

    #[test]
    fn engine_for_resumed_session_succeeds_when_all_paths_present() {
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        std::fs::write(&a, b"hi\n").unwrap();
        std::fs::write(&b, b"").unwrap();

        let mut s = Session::new();
        s.sources
            .insert_unique(SessionSource {
                id: SourceId::from(
                    a.canonicalize_utf8().unwrap().as_str().to_string(),
                ),
                path: a.canonicalize_utf8().unwrap(),
                mtime: chrono::Utc::now(),
                size: 3,
            })
            .unwrap();
        s.sources
            .insert_unique(SessionSource {
                id: SourceId::from(
                    b.canonicalize_utf8().unwrap().as_str().to_string(),
                ),
                path: b.canonicalize_utf8().unwrap(),
                mtime: chrono::Utc::now(),
                size: 0,
            })
            .unwrap();
        let Ok(_engine) = engine_for_resumed_session(&s) else {
            panic!("expected engine_for_resumed_session to succeed");
        };
        dir.cleanup();
    }

    #[test]
    fn load_all_sessions_sorts_newest_first_and_skips_corrupt() {
        let dir = TestDir::new();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();

        // Save three sessions with explicit timestamps.
        let make = |secs: i64| {
            let mut s = Session::new();
            s.last_saved_at =
                chrono::TimeZone::timestamp_opt(&chrono::Utc, secs, 0)
                    .single()
                    .unwrap();
            s
        };
        let old = make(100);
        let mid = make(200);
        let new = make(300);
        store.save(old.id, &old).unwrap();
        store.save(mid.id, &mid).unwrap();
        store.save(new.id, &new).unwrap();

        // Drop a corrupt JSON named like a session id; it should
        // be silently skipped.
        std::fs::write(
            store.sessions_dir().join("deadbeef.json"),
            "{ not valid",
        )
        .unwrap();

        let sessions = load_all_sessions(&store).unwrap();
        let ids: Vec<_> = sessions.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![new.id, mid.id, old.id]);
        dir.cleanup();
    }

    // ---------- startup dialog (phase 8) ----------

    fn fake_match(kind: MatchKind) -> SessionMatch {
        SessionMatch { kind, session: Session::new() }
    }

    fn keypress(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn startup_dialog_with_no_matches_has_two_rows() {
        let d = StartupDialog::new(Vec::new());
        assert_eq!(d.rows(), 2);
        assert_eq!(d.new_saved_idx(), 0);
        assert_eq!(d.new_transient_idx(), 1);
        assert_eq!(d.selected, 0);
    }

    #[test]
    fn startup_dialog_navigation_clamps_at_ends() {
        let mut d = StartupDialog::new(vec![
            fake_match(MatchKind::Exact),
            fake_match(MatchKind::Overlap),
        ]);
        // rows = 2 candidates + 2 fixed = 4
        assert_eq!(d.rows(), 4);

        // Pressing k at the top is a no-op.
        d.move_up();
        assert_eq!(d.selected, 0);

        for expected in 1..4 {
            d.move_down();
            assert_eq!(d.selected, expected);
        }
        // Pressing j past the bottom is a no-op.
        d.move_down();
        assert_eq!(d.selected, 3);
    }

    #[test]
    fn startup_dialog_confirm_at_each_index() {
        let make = || {
            StartupDialog::new(vec![
                fake_match(MatchKind::Exact),
                fake_match(MatchKind::Overlap),
            ])
        };

        // Row 0: first candidate.
        let mut d = make();
        d.selected = 0;
        assert!(matches!(d.confirm(), StartupChoice::ResumeSavedSession(_)));

        // Row 1: second candidate.
        let mut d = make();
        d.selected = 1;
        assert!(matches!(d.confirm(), StartupChoice::ResumeSavedSession(_)));

        // Row 2: New saved.
        let mut d = make();
        d.selected = 2;
        assert!(matches!(d.confirm(), StartupChoice::NewSavedSession));

        // Row 3: New transient.
        let mut d = make();
        d.selected = 3;
        assert!(matches!(d.confirm(), StartupChoice::NewTransientSession));
    }

    #[test]
    fn startup_dialog_esc_returns_quit() {
        let d = StartupDialog::new(Vec::new());
        match d.handle_key(keypress(KeyCode::Esc)) {
            StartupDialogStep::Done(StartupChoice::Quit) => {}
            other => {
                panic!(
                    "expected Done(Quit), got something else: {}",
                    match other {
                        StartupDialogStep::Continue(_) => "Continue",
                        StartupDialogStep::Done(_) => "Done(non-Quit)",
                    }
                );
            }
        }
    }

    #[test]
    fn startup_dialog_ctrl_c_returns_quit() {
        let d = StartupDialog::new(Vec::new());
        match d.handle_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )) {
            StartupDialogStep::Done(StartupChoice::Quit) => {}
            _ => panic!("expected Done(Quit)"),
        }
    }

    #[test]
    fn startup_dialog_enter_at_default_picks_first_candidate() {
        // When there are candidates, `selected` defaults to 0 (the
        // first candidate); Enter resumes it.
        let d = StartupDialog::new(vec![fake_match(MatchKind::Exact)]);
        match d.handle_key(keypress(KeyCode::Enter)) {
            StartupDialogStep::Done(StartupChoice::ResumeSavedSession(_)) => {}
            _ => panic!("expected Done(Resume)"),
        }
    }

    #[test]
    fn startup_dialog_enter_with_no_candidates_picks_new_saved() {
        // Empty matches => selected=0 maps to NewSaved (no candidate
        // rows in between).
        let d = StartupDialog::new(Vec::new());
        match d.handle_key(keypress(KeyCode::Enter)) {
            StartupDialogStep::Done(StartupChoice::NewSavedSession) => {}
            _ => panic!("expected Done(NewSaved)"),
        }
    }

    #[test]
    fn startup_dialog_digit_shortcut_jumps_to_candidate() {
        let d = StartupDialog::new(vec![
            fake_match(MatchKind::Exact),
            fake_match(MatchKind::Superset),
            fake_match(MatchKind::Overlap),
        ]);
        // '2' jumps to the second candidate (index 1).
        let d = match d.handle_key(keypress(KeyCode::Char('2'))) {
            StartupDialogStep::Continue(d) => d,
            _ => panic!("expected Continue"),
        };
        assert_eq!(d.selected, 1);
    }

    #[test]
    fn startup_dialog_digit_shortcut_out_of_range_is_ignored() {
        let d = StartupDialog::new(vec![fake_match(MatchKind::Exact)]);
        // '5' is out of range (only one candidate); selection unchanged.
        let d = match d.handle_key(keypress(KeyCode::Char('5'))) {
            StartupDialogStep::Continue(d) => d,
            _ => panic!("expected Continue"),
        };
        assert_eq!(d.selected, 0);
    }

    #[test]
    fn render_startup_dialog_does_not_panic_on_empty_or_populated() {
        // We don't assert on pixels — just that the render path
        // handles both shapes without panicking.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        let dialog = StartupDialog::new(Vec::new());
        terminal.draw(|frame| render_startup_dialog(frame, &dialog)).unwrap();

        let dialog = StartupDialog::new(vec![
            fake_match(MatchKind::Exact),
            fake_match(MatchKind::Superset),
            fake_match(MatchKind::Overlap),
        ]);
        terminal.draw(|frame| render_startup_dialog(frame, &dialog)).unwrap();
    }

    // ---------- debounced persistence (phase 7) ----------

    /// Subtracts `delta` from "now" without ever underflowing — used
    /// to rewind a policy's `last_saved_at` so a test can pretend
    /// the debounce window has elapsed without sleeping.
    fn now_minus(delta: Duration) -> Instant {
        let now = Instant::now();
        now.checked_sub(delta).unwrap_or(now)
    }

    #[test]
    fn j_scroll_records_debounced() {
        let (mut a, dir) = app_with_store_and_one_tab();
        // The initial setup flushed the dirty bit.
        assert!(!a.policy.dirty());
        a.handle_key(key(KeyCode::Char('j')));
        assert!(a.policy.dirty(), "j should have marked the policy dirty");
        dir.cleanup();
    }

    #[test]
    fn ctrl_d_scroll_records_debounced() {
        let (mut a, dir) = app_with_store_and_one_tab();
        assert!(!a.policy.dirty());
        a.handle_key(ctrl('d'));
        assert!(a.policy.dirty());
        dir.cleanup();
    }

    #[test]
    fn flush_if_due_flushes_when_window_has_elapsed() {
        let (mut a, dir) = app_with_store_and_one_tab();
        // Pretend the last save happened well outside the debounce
        // window.
        a.policy.mark_saved(now_minus(Duration::from_secs(60)));
        a.policy.record(seer::Cadence::Debounced);
        assert!(a.policy.dirty());

        a.flush_if_due();
        assert!(
            !a.policy.dirty(),
            "flush_if_due should have cleared the dirty bit"
        );
        assert!(a.notice.is_none(), "successful flush should not set a notice");
        dir.cleanup();
    }

    #[test]
    fn flush_if_due_is_a_noop_when_clean() {
        let (mut a, dir) = app_with_store_and_one_tab();
        // Even with the window long elapsed, a clean policy stays
        // clean and no save fires.
        a.policy.mark_saved(now_minus(Duration::from_secs(60)));
        assert!(!a.policy.dirty());
        a.flush_if_due();
        assert!(!a.policy.dirty());
        dir.cleanup();
    }

    #[test]
    fn flush_if_due_is_a_noop_within_debounce_window() {
        let (mut a, dir) = app_with_store_and_one_tab();
        // Just-saved + just-recorded: the window has not elapsed.
        a.policy.mark_saved(Instant::now());
        a.policy.record(seer::Cadence::Debounced);
        assert!(a.policy.dirty());
        a.flush_if_due();
        assert!(
            a.policy.dirty(),
            "dirty bit must persist when the window hasn't elapsed"
        );
        dir.cleanup();
    }

    #[test]
    fn flush_if_due_failure_sets_notice_and_keeps_dirty_bit() {
        // Yank the sessions directory so the save attempt errors.
        // The debounce check fires, the save fails, the error lands
        // on `notice`, and the dirty bit stays set so the next
        // opportunity retries.
        let dir = TestDir::new();
        let sessions_dir = dir.path().join("sessions");
        let store = SessionStore::open_at(&sessions_dir).unwrap();
        let mut a = App::new_for_tests(Engine::new());
        a.store = Some(store);
        // Make the policy due.
        a.policy.mark_saved(now_minus(Duration::from_secs(60)));
        a.policy.record(seer::Cadence::Debounced);
        // Now break the store.
        std::fs::remove_dir_all(&sessions_dir).unwrap();

        a.flush_if_due();
        assert!(a.policy.dirty(), "failed save leaves dirty bit set");
        let notice = a.notice.as_ref().expect("notice must be set");
        assert!(
            notice.contains("session save failed"),
            "unexpected notice: {notice}"
        );
        dir.cleanup();
    }

    #[test]
    fn viewport_resize_in_render_records_debounced() {
        let (mut a, dir) = app_with_store_and_one_tab();
        // First render lands the 0→N transition; let it settle and
        // then mark_saved so subsequent dirty signals are isolated.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        a.policy.mark_saved(Instant::now());
        assert!(!a.policy.dirty());

        // Resize to a different geometry and re-render.
        terminal.backend_mut().resize(40, 12);
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        assert!(
            a.policy.dirty(),
            "resize should have recorded a debounced mutation"
        );
        dir.cleanup();
    }

    #[test]
    fn render_at_same_size_does_not_record() {
        let (mut a, dir) = app_with_store_and_one_tab();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        a.policy.mark_saved(Instant::now());
        assert!(!a.policy.dirty());

        // Re-render at the same size: no resize, no record.
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        assert!(!a.policy.dirty(), "render at the same size must not record");
        dir.cleanup();
    }

    #[test]
    fn inline_save_failure_sets_notice_and_keeps_dirty_bit() {
        // Attach a store, then remove the underlying directory so the
        // next save fails.  The mutation should still proceed; the
        // save error should land on `notice`; and the dirty bit
        // should stay set so future opportunities can retry.
        let dir = TestDir::new();
        let sessions_dir = dir.path().join("sessions");
        let store = SessionStore::open_at(&sessions_dir).unwrap();
        let mut a = App::new_for_tests(Engine::new());
        a.store = Some(store);
        // Save once successfully so the initial state is on disk.
        a.save_after_inline_mutation();
        assert!(a.notice.is_none());

        // Yank the directory out from under the store.  Subsequent
        // saves will hit ENOENT or similar.
        std::fs::remove_dir_all(&sessions_dir).unwrap();

        let stream_id = a.tabs[a.active].stream;
        let draft = BookmarkDraft {
            cursor: Cursor::with([]),
            display_source: SourceId::from("test".to_string()),
            display_time: chrono::Utc::now(),
            display_name: "Nexus".to_string(),
            display_msg: "marked".to_string(),
        };
        a.add_bookmark(None, draft);

        assert!(a.policy.dirty(), "failed save leaves dirty bit set");
        let notice = a.notice.as_ref().expect("notice must be set on failure");
        assert!(
            notice.contains("session save failed"),
            "unexpected notice: {notice}"
        );
        // The mutation itself still landed in memory.
        assert!(a.session.user_bookmarks.contains_key(&stream_id));
        dir.cleanup();
    }

    #[test]
    fn capital_y_opens_seeit_command_dialog() {
        let (mut a, dir) = app_with_store_and_one_tab();
        let session_id = a.session.id;
        let tab_name = a.active_tab().name.clone();

        a.handle_key(shift('Y'));

        // The dialog now carries the seeit-command popup with the
        // exact reproduction string for the active tab.
        let Some(Dialog::SeeitCommand { text }) = &a.dialog else {
            panic!("expected SeeitCommand dialog after Y");
        };
        let expected = format!(
            "seeit --session {session_id} --tab {}",
            shlex::try_quote(&tab_name).unwrap(),
        );
        assert_eq!(text, &expected);
        dir.cleanup();
    }

    #[test]
    fn esc_closes_seeit_command_dialog() {
        let (mut a, dir) = app_with_store_and_one_tab();
        a.handle_key(shift('Y'));
        assert!(matches!(a.dialog, Some(Dialog::SeeitCommand { .. })));

        a.handle_key(key(KeyCode::Esc));
        assert!(a.dialog.is_none());
        dir.cleanup();
    }

    #[test]
    fn enter_closes_seeit_command_dialog() {
        // Read-only popup: Enter should close it (same as Esc), not
        // act on the underlying tab.
        let (mut a, dir) = app_with_store_and_one_tab();
        a.handle_key(shift('Y'));
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_none());
        dir.cleanup();
    }

    #[test]
    fn seeit_command_dialog_ignores_random_keys() {
        // Non-Esc/Enter keys should not fall through to the
        // underlying tab (which would scroll or quit) while the popup
        // is open.
        let (mut a, dir) = app_with_store_and_one_tab();
        a.handle_key(shift('Y'));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('q')));
        assert!(matches!(a.dialog, Some(Dialog::SeeitCommand { .. })));
        assert!(!a.quit);
        dir.cleanup();
    }

    #[test]
    fn seeit_command_dialog_saves_session_before_opening() {
        // The opening flow calls try_save_now so the on-disk session
        // matches the user's current state.  Use a save-policy
        // mutation that hasn't been flushed yet, then press Y, and
        // assert the dirty bit cleared.
        let (mut a, dir) = app_with_store_and_one_tab();
        a.policy.record(Cadence::Inline);
        // App::new + the constructor's initial save left the policy
        // clean; we just dirtied it.  Confirm.
        assert!(a.policy.dirty());

        a.handle_key(shift('Y'));
        assert!(
            !a.policy.dirty(),
            "Y should have flushed the dirty bit via try_save_now",
        );
        dir.cleanup();
    }

    #[test]
    fn seeit_command_dialog_renders_visibly() {
        // Regression: an earlier version of the SeeitCommand variant
        // forgot to add itself to the centered-popup dispatch in
        // `render`, leaving the dialog invisible while still
        // capturing every keystroke.  Drive a full render and
        // assert the popup's title text appears in the buffer.
        let (mut a, dir) = app_with_store_and_one_tab();
        // Push enough rows that the underlying tab has content to
        // render under the popup; without rows the assert still
        // catches the missing dispatch, but the dump is easier to
        // read with realistic content.
        let backend = TestBackend::new(200, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        a.handle_key(shift('Y'));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let dump = buffer_text(terminal.backend().buffer());
        assert!(
            dump.contains("seeit reproduction"),
            "expected popup title in rendered buffer, got:\n{dump}",
        );
        assert!(
            dump.contains("seeit --session"),
            "expected command text in rendered buffer, got:\n{dump}",
        );
        dir.cleanup();
    }

    #[test]
    fn seeit_command_dialog_notice_when_session_is_transient() {
        // App with no store attached (the transient-session case).
        let mut a = App::new_for_tests(Engine::new());
        assert!(a.store.is_none());

        a.handle_key(shift('Y'));

        assert!(a.dialog.is_none());
        let notice = a.notice.as_ref().expect("notice should be set");
        assert!(
            notice.contains("transient"),
            "expected transient-session notice, got: {notice}",
        );
    }

    /// Adds `n` named bookmarks to the App's first stream and cycles
    /// the active pane onto the synthetic Bookmarks tab.  Bookmarks
    /// are inserted via [`add_bookmark_at`] so each one's
    /// `display_time` is unique and the order in `flat_bookmarks`
    /// matches the insertion order's ascending second-counts.
    fn enter_bookmarks_pane_with(a: &mut App, n: usize) {
        let stream_id = a.tabs[a.active].stream;
        for i in 0..n {
            add_bookmark_at(a, stream_id, (i as i64) + 1, &format!("bm-{i}"));
        }
        // Cycle to the Bookmarks pane: it lives at index `tabs.len()`
        // in the virtual pane list, which is one Tab past every
        // regular tab.
        while !a.bookmarks_active() {
            a.handle_key(key(KeyCode::Tab));
        }
    }

    #[test]
    fn capital_y_in_bookmarks_opens_seeit_for_cursor() {
        // The Bookmarks pane's cursor *is* the selection — `Y` should
        // pop the reproduction command for the highlighted bookmark
        // without a separate "arm" step.
        let (mut a, dir) = app_with_store_and_one_tab();
        enter_bookmarks_pane_with(&mut a, 3);
        let session_id = a.session.id;
        // Walk the cursor from "unset" to bookmark 2 so the popup
        // targets a specific row rather than the first one.
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Char('j')));
        let cursor_id = a.bookmark_cursor.expect("cursor should be set");

        a.handle_key(shift('Y'));

        let Some(Dialog::SeeitCommand { text }) = &a.dialog else {
            panic!("expected SeeitCommand dialog after Y");
        };
        let expected = format!(
            "seeit --session {session_id} --bookmark {}",
            shlex::try_quote(&cursor_id.to_string()).unwrap(),
        );
        assert_eq!(text, &expected);
        dir.cleanup();
    }

    #[test]
    fn capital_y_in_bookmarks_without_shift_modifier_also_works() {
        // Mirrors the regular-tab binding: terminals report `Y` with
        // either NONE or SHIFT and both should open the popup.
        let (mut a, dir) = app_with_store_and_one_tab();
        enter_bookmarks_pane_with(&mut a, 1);

        a.handle_key(key(KeyCode::Char('Y')));

        assert!(matches!(a.dialog, Some(Dialog::SeeitCommand { .. })));
        dir.cleanup();
    }

    #[test]
    fn capital_y_in_bookmarks_initializes_cursor_when_unset() {
        // The Bookmarks pane doesn't auto-highlight the first row on
        // entry — pressing `Y` before any j/k should still target
        // *some* bookmark rather than silently no-op.
        let (mut a, dir) = app_with_store_and_one_tab();
        enter_bookmarks_pane_with(&mut a, 2);
        assert!(a.bookmark_cursor_idx().is_none());

        a.handle_key(shift('Y'));

        assert_eq!(a.bookmark_cursor_idx(), Some(0));
        assert!(matches!(a.dialog, Some(Dialog::SeeitCommand { .. })));
        dir.cleanup();
    }

    #[test]
    fn bookmarks_seeit_uses_bookmark_id_not_name() {
        // Two bookmarks with the same name would be ambiguous if we
        // emitted `--bookmark <name>`; the id form is always unique.
        // Guard against a regression that picks the human-readable
        // name instead.
        let (mut a, dir) = app_with_store_and_one_tab();
        enter_bookmarks_pane_with(&mut a, 1);
        let cursor_id = a.flat_bookmarks()[0].id;

        a.handle_key(shift('Y'));

        let Some(Dialog::SeeitCommand { text }) = &a.dialog else {
            panic!("expected SeeitCommand dialog");
        };
        assert!(
            text.contains(&cursor_id.to_string()),
            "expected bookmark id {} in command, got: {}",
            cursor_id,
            text,
        );
        // The name `bm-0` shouldn't end up as the selector either.
        assert!(
            !text.contains("bm-0"),
            "command should not embed the bookmark name, got: {text}",
        );
        dir.cleanup();
    }

    #[test]
    fn bookmarks_enter_still_navigates() {
        // Regression guard: the Y binding must not have stolen Enter's
        // default semantics on the Bookmarks pane.
        let (mut a, dir) = app_with_store_and_one_tab();
        enter_bookmarks_pane_with(&mut a, 1);
        // Initialize the bookmark cursor — `navigate_to_bookmark_cursor`
        // is a no-op while it's unset, which would mask the assertion
        // below.
        a.handle_key(key(KeyCode::Char('j')));

        a.handle_key(key(KeyCode::Enter));

        assert!(
            !matches!(a.dialog, Some(Dialog::SeeitCommand { .. })),
            "Enter must not open the seeit popup",
        );
        assert!(!a.bookmarks_active(), "Enter must have navigated away");
        dir.cleanup();
    }

    #[test]
    fn bookmarks_y_with_no_store_falls_back_to_notice() {
        // Same transient-session story as the main-tab Y binding: no
        // store means no on-disk session for `seeit` to point at.
        let mut a = App::new_for_tests(Engine::new());
        let stream_id = a.tabs[a.active].stream;
        add_bookmark_at(&mut a, stream_id, 1, "only");
        while !a.bookmarks_active() {
            a.handle_key(key(KeyCode::Tab));
        }

        a.handle_key(shift('Y'));

        assert!(a.dialog.is_none());
        let notice = a.notice.as_ref().expect("notice should be set");
        assert!(
            notice.contains("transient"),
            "expected transient-session notice, got: {notice}",
        );
    }
}
