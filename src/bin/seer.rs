// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `seer`: minimal interactive log viewer.
//!
//! Builds a [`seer::Engine`] from the file paths on the command line and
//! presents one or more tabs over it.  Each [`Tab`] is an independent
//! view with its own [`Filter`] and scroll position; the engine itself
//! (and therefore the underlying sources) is shared.  Future iterations
//! will lazy-load and wire in bookmarks and richer log streams; this is
//! the smallest end-to-end exercise of the parse → engine → render
//! path.

use camino::Utf8PathBuf;
use clap::Parser;
use ratatui::Frame;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Tabs};
use regex::Regex;
#[cfg(test)]
use seer::Event as LogEvent;
#[cfg(test)]
use seer::SourceId;
use seer::{
    Bookmark, BookmarkId, BookmarkName, Engine, EngineEvent, Filter, LogStream,
    LogStreamId, LogStreamPosition, Predicate, ResolvePosition, Session,
    format_event,
};
use std::time::Duration;

#[derive(Parser)]
#[command(about = "interactive log explorer")]
struct Args {
    /// One or more bunyan log files to read, in order.
    #[arg(required = true)]
    files: Vec<Utf8PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let mut engine = Engine::new();
    for path in &args.files {
        engine.add_file_source(path)?;
    }

    let session = load_session();
    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;
    let mut app = App::new_with_session(engine, session);
    while !app.quit {
        terminal.draw(|frame| render(frame, &mut app))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(key);
        }
    }
    if let Err(err) = save_session(&app.session) {
        // Best-effort: failing to persist shouldn't prevent quit.  The
        // user has already asked to leave; surface the failure on
        // stderr so they can investigate after the TUI tears down.
        eprintln!("warning: failed to save session: {err}");
    }
    Ok(())
}

/// Path used for the persisted [`Session`].  Returns `None` when
/// `$HOME` is unset, in which case persistence is silently skipped.
/// We intentionally don't fall back to `/tmp` or similar; a missing
/// `$HOME` means we don't know where the user wants their state.
fn session_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".seer").join("session.json"))
}

/// Loads the persisted session, falling back to an empty one on any
/// failure (missing file, unreadable, parse error).  Failures are
/// surfaced to stderr but never abort startup — bookmarks and (later)
/// open tabs are nice-to-have, not critical, and a corrupt session
/// shouldn't lock the user out of the tool.
fn load_session() -> Session {
    let Some(path) = session_path() else {
        return Session::new();
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Session::new();
        }
        Err(err) => {
            eprintln!("warning: could not read {}: {err}", path.display());
            return Session::new();
        }
    };
    match serde_json::from_str::<Session>(&contents) {
        Ok(s) => s,
        Err(err) => {
            eprintln!(
                "warning: ignoring unreadable session file {}: {err}",
                path.display()
            );
            Session::new()
        }
    }
}

/// Writes `session` to [`session_path`] in pretty-printed JSON,
/// creating the parent directory if needed.  Errors propagate; the
/// caller decides how to surface them.
fn save_session(session: &Session) -> std::io::Result<()> {
    let Some(path) = session_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(session)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, json)
}

/// Restores the terminal on drop so panics and `?`-returns don't leave
/// the user's shell in raw mode / alt-screen.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// Materializes the active filter against the engine, returning the
/// records alongside their multi-line display rendering.
///
/// `events` is one entry per record produced by [`Engine::query_events`]:
/// `Some(EngineEvent)` for an `Ok` result, `None` for a parse/I/O error
/// or out-of-order warning.  `formatted` is one entry per *display line*
/// — an event with `n` extra fields contributes `1 + n` lines (header
/// plus indented `key = value` rows), and an error contributes a single
/// line carrying its `Display` message.  `formatted.len() >=
/// events.len()`, with equality only when no event has any extras.
///
/// `event_for_line[i]` is the index into `events` of the record that
/// produced display line `i`.  `first_line_for_event[i]` is the inverse:
/// the first line index for record `i`.  Together they let callers
/// translate freely between "scroll position" (a line index) and "the
/// record under the cursor" (an event index) without rescanning.
struct RenderedRows {
    events: Vec<Option<EngineEvent>>,
    formatted: Vec<String>,
    event_for_line: Vec<usize>,
    first_line_for_event: Vec<usize>,
}

fn render_rows(
    engine: &Engine,
    filter: &Filter,
    show_extras: bool,
) -> RenderedRows {
    let mut events = Vec::new();
    let mut formatted = Vec::new();
    let mut event_for_line = Vec::new();
    let mut first_line_for_event = Vec::new();
    for r in engine.query_events(filter) {
        let event_idx = events.len();
        first_line_for_event.push(formatted.len());
        match r {
            Ok(ee) => {
                let lines = format_event(&ee.event, show_extras);
                for line in lines {
                    formatted.push(line);
                    event_for_line.push(event_idx);
                }
                events.push(Some(ee));
            }
            Err(err) => {
                // SourceError's Display already says "I/O error: ...",
                // "failed to parse ...", or "warning: ..." as
                // appropriate; don't add another prefix.
                formatted.push(err.to_string());
                event_for_line.push(event_idx);
                events.push(None);
            }
        }
    }
    RenderedRows { events, formatted, event_for_line, first_line_for_event }
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
    fn prompt(self) -> char {
        match self {
            Self::Forward => '/',
            Self::Backward => '?',
        }
    }
}

/// State of the active search on a tab.  `matches` is sorted ascending
/// and is the list of row indices in [`Tab::rows`] where `regex` finds
/// at least one match.  Cleared whenever `rows` changes (e.g. after a
/// filter edit), since the indices would no longer line up.
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
/// `events` is one entry per record (an event or an error); `formatted`
/// is one entry per *display line*, where a single event with extra
/// fields contributes its header plus one indented `key = value` line
/// per extra.  `event_for_line` and `first_line_for_event` translate
/// between the two indexings: the former says which record produced a
/// given display line, the latter says where each record's first line
/// lives.  `events[i]` is `None` exactly when that record came from a
/// parse or I/O error rather than a real log record; the stringified
/// error sits in `formatted[first_line_for_event[i]]`.  Keeping the
/// parsed events around lets in-place actions (e.g. exclude mode's
/// "filter out entries like this" or `b`'s "bookmark this entry")
/// inspect the record under the cursor without re-parsing.
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
    events: Vec<Option<EngineEvent>>,
    formatted: Vec<String>,
    /// `event_for_line[line] = event_idx`.  `formatted.len()` long.
    event_for_line: Vec<usize>,
    /// `first_line_for_event[event] = line`.  `events.len()` long.
    /// Maintained so the (event_idx → line_idx) translation is O(1) on
    /// every selection move and bookmark navigation.
    first_line_for_event: Vec<usize>,
    /// Index of the *display line* at the top of the viewport.  The
    /// viewport scrolls in line steps so users can see (and search) the
    /// extra-field rows independently from their headers.
    viewport_top: usize,
    /// Active highlighted search, if any.  Match indices are line
    /// indices into `formatted`; cleared when the rows are re-queried
    /// (filter change), because the indices would otherwise dangle.
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

/// What a select-mode commit will do.
///
/// `x` → exclude (build `msg != <selected>`); `X` → include (build
/// `msg = <selected>`); `b` → bookmark (open the name dialog).  Stored
/// on [`Selection`] so the rendering and dispatch paths agree on what
/// kind of mode the user is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionAction {
    Exclude,
    Include,
    Bookmark,
}

/// State of an in-progress `x`/`X`/`b` selection.
///
/// `event_idx` is an index into [`Tab::events`] (i.e., a record, not a
/// display line).  `action` is what Enter will do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    event_idx: usize,
    action: SelectionAction,
}

impl Tab {
    fn new(
        name: String,
        engine: &Engine,
        stream: LogStreamId,
        filter: &Filter,
        show_extras: bool,
    ) -> Self {
        let rendered = render_rows(engine, filter, show_extras);
        Self {
            name,
            stream,
            events: rendered.events,
            formatted: rendered.formatted,
            event_for_line: rendered.event_for_line,
            first_line_for_event: rendered.first_line_for_event,
            viewport_top: 0,
            search: None,
            select: None,
        }
    }

    /// Re-runs the host stream's filter against the engine and refreshes
    /// the cached rows, viewport, and transient selection/search state.
    /// Call after the [`LogStream::filter`] for `self.stream` has been
    /// mutated.
    fn refresh(&mut self, engine: &Engine, filter: &Filter, show_extras: bool) {
        let rendered = render_rows(engine, filter, show_extras);
        self.events = rendered.events;
        self.formatted = rendered.formatted;
        self.event_for_line = rendered.event_for_line;
        self.first_line_for_event = rendered.first_line_for_event;
        self.viewport_top = 0;
        self.search = None;
        self.select = None;
    }

    /// Re-renders the host stream like [`Self::refresh`], but keeps the
    /// viewport pinned to the *record* that was at the top before — used
    /// when only the rendering changed (e.g. toggling `show_extras`),
    /// where the underlying events are the same and resetting to the top
    /// would lose the user's place.  Search is cleared because match
    /// indices are line-indexed and lines moved; selection is preserved
    /// because it sits on a record index, which is still valid.
    fn rerender(
        &mut self,
        engine: &Engine,
        filter: &Filter,
        show_extras: bool,
    ) {
        let anchor_event = self.event_for_line.get(self.viewport_top).copied();
        let rendered = render_rows(engine, filter, show_extras);
        self.events = rendered.events;
        self.formatted = rendered.formatted;
        self.event_for_line = rendered.event_for_line;
        self.first_line_for_event = rendered.first_line_for_event;
        self.viewport_top = anchor_event
            .and_then(|i| self.first_line_for_event.get(i).copied())
            .unwrap_or(0);
        self.search = None;
    }

    /// Last *display line* index belonging to record `event_idx`,
    /// inclusive.  A header-only record has its first and last on the
    /// same line.
    fn last_line_for_event(&self, event_idx: usize) -> usize {
        let next_first = self
            .first_line_for_event
            .get(event_idx + 1)
            .copied()
            .unwrap_or(self.formatted.len());
        // `next_first` is exclusive; the last line for this event is
        // one before it.  Records always contribute at least one line
        // so the subtraction never underflows.
        next_first - 1
    }

    /// Largest valid `viewport_top`: the line index that places the last
    /// line of `formatted` flush with the bottom of the viewport.
    fn max_top(&self, viewport_height: u16) -> usize {
        self.formatted.len().saturating_sub(viewport_height as usize)
    }

    fn scroll_down(&mut self, n: usize, viewport_height: u16) {
        let max = self.max_top(viewport_height);
        self.viewport_top = (self.viewport_top + n).min(max);
    }

    fn scroll_up(&mut self, n: usize) {
        self.viewport_top = self.viewport_top.saturating_sub(n);
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
        if self.events.is_empty() {
            return;
        }
        let last = self.events.len() - 1;
        let new_idx =
            (sel.event_idx as isize + delta).clamp(0, last as isize) as usize;
        self.select = Some(Selection { event_idx: new_idx, ..sel });
        let first = self.first_line_for_event[new_idx];
        let last_line = self.last_line_for_event(new_idx);
        let height = viewport_height as usize;
        if first < self.viewport_top {
            self.viewport_top = first;
        } else if height > 0 && last_line >= self.viewport_top + height {
            let event_height = last_line - first + 1;
            self.viewport_top = if event_height >= height {
                first
            } else {
                last_line + 1 - height
            };
        }
    }

    /// Record index of the closest event to `viewport_top` in the
    /// requested direction.  Falls back to the opposite direction so a
    /// viewport parked on an error row at one end of the file still gets
    /// an anchor; returns `None` only when there are no parsed events
    /// at all.  Used by [`Self::advance_time`] to decide what timestamp
    /// to add the step to.
    fn time_anchor_idx(&self, prefer_forward: bool) -> Option<usize> {
        // Translate the line-indexed viewport_top to its enclosing
        // record so the search range matches the user's visual
        // position.  When the viewport is parked past the last line
        // (only possible if `formatted` is empty, in which case events
        // is too) `event_for_line` would index out of range — check
        // length first.
        let pivot = if self.viewport_top < self.event_for_line.len() {
            self.event_for_line[self.viewport_top]
        } else {
            self.events.len()
        };
        let forward = self
            .events
            .iter()
            .enumerate()
            .skip(pivot)
            .find_map(|(i, e)| e.as_ref().map(|_| i));
        let backward_take = pivot.saturating_add(1).min(self.events.len());
        let backward = self
            .events
            .iter()
            .enumerate()
            .take(backward_take)
            .rev()
            .find_map(|(i, e)| e.as_ref().map(|_| i));
        if prefer_forward { forward.or(backward) } else { backward.or(forward) }
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
    fn advance_time(&mut self, delta: chrono::Duration, viewport_height: u16) {
        let go_forward = delta.num_milliseconds() > 0;
        let Some(anchor_idx) = self.time_anchor_idx(go_forward) else {
            return;
        };
        let anchor_time = self.events[anchor_idx]
            .as_ref()
            .expect("time_anchor_idx returns indices of real events")
            .event
            .time;
        let target = anchor_time + delta;
        let max = self.max_top(viewport_height);
        let new_event = if go_forward {
            self.events.iter().enumerate().skip(anchor_idx).find_map(
                |(i, e)| {
                    e.as_ref().filter(|ee| ee.event.time >= target).map(|_| i)
                },
            )
        } else {
            self.events.iter().enumerate().take(anchor_idx + 1).rev().find_map(
                |(i, e)| {
                    e.as_ref().filter(|ee| ee.event.time <= target).map(|_| i)
                },
            )
        };
        let new_top = match new_event {
            Some(idx) => self.first_line_for_event[idx],
            None if go_forward => max,
            None => 0,
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
}

impl App {
    /// Convenience constructor for tests that don't care about the
    /// session.  Production code goes through [`Self::new_with_session`]
    /// so a previously-saved session is honored.
    #[cfg(test)]
    fn new(engine: Engine) -> Self {
        Self::new_with_session(engine, Session::new())
    }

    /// Constructs an [`App`] reusing a previously-loaded [`Session`].
    /// Carries over the user's bookmarks (and the streams those
    /// bookmarks reference), but always opens a fresh display tab —
    /// auto-restoring the user's prior tab set is a separate piece of
    /// work, and dropping into an unfiltered fresh tab is a sensible
    /// default until then.  Loaded session may include `tabs` from a
    /// future schema; we leave that field intact so a later
    /// auto-resume can pick it up without a save round-trip
    /// re-emptying it.
    fn new_with_session(engine: Engine, session: Session) -> Self {
        let mut a = Self {
            engine,
            session,
            tabs: Vec::new(),
            active: 0,
            next_tab_number: 1,
            viewport_height: 0,
            quit: false,
            dialog: None,
            last_search: None,
            time_step_idx: DEFAULT_TIME_STEP_IDX,
            bookmark_cursor: None,
            notice: None,
        };
        a.push_tab(Filter::default());
        a
    }

    /// Pushes a new tab backed by a fresh [`LogStream`] with the given
    /// filter.  Does *not* open the filter dialog — callers that want
    /// that (e.g. Ctrl-T) do it explicitly after.  Switches focus to
    /// the new tab.
    fn push_tab(&mut self, filter: Filter) {
        let name = format!("Tab {}", self.next_tab_number);
        self.next_tab_number += 1;
        let mut stream = LogStream::new(name.clone());
        stream.filter = filter;
        let stream_id = stream.id;
        self.session
            .streams
            .insert_unique(stream)
            .expect("freshly-minted LogStreamId is unique");
        let stream = self.session.streams.get(&stream_id).unwrap();
        let tab = Tab::new(
            name,
            &self.engine,
            stream_id,
            &stream.filter,
            stream.show_extras,
        );
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

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
            &self.engine,
            stream_id,
            &stream.filter,
            stream.show_extras,
        );
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
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
    /// and resets every tab targeting that stream to the top.  Two
    /// tabs sharing a stream therefore share their filter — that's the
    /// model that lets a bookmark-driven "open in a new tab" carry the
    /// stream's filter forward.
    fn apply_filter(&mut self, filter: Filter) {
        let stream_id = self.tabs[self.active].stream;
        // Mutate the persisted filter, then re-derive every tab whose
        // stream is the one we just changed.
        let Some(mut stream) = self.session.streams.remove(&stream_id) else {
            return;
        };
        stream.filter = filter;
        let new_filter = stream.filter.clone();
        let show_extras = stream.show_extras;
        self.session
            .streams
            .insert_unique(stream)
            .expect("removed-then-reinserted id is unique");
        for tab in self.tabs.iter_mut() {
            if tab.stream == stream_id {
                tab.refresh(&self.engine, &new_filter, show_extras);
            }
        }
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
        let new_filter = stream.filter.clone();
        let show_extras = stream.show_extras;
        self.session
            .streams
            .insert_unique(stream)
            .expect("removed-then-reinserted id is unique");
        for tab in self.tabs.iter_mut() {
            if tab.stream == stream_id {
                tab.rerender(&self.engine, &new_filter, show_extras);
            }
        }
    }

    /// Returns the active stream's filter.  Convenience for the few
    /// places that need to display or clone it (footer, dialog
    /// pre-fill, Ctrl-T new-tab).
    fn active_filter(&self) -> &Filter {
        let stream_id = self.tabs[self.active].stream;
        &self.session.streams.get(&stream_id).expect("stream exists").filter
    }

    /// Returns whether the active stream is currently rendering
    /// structured-field extras.  Used by the footer to show the F-key
    /// state.
    fn active_show_extras(&self) -> bool {
        let stream_id = self.tabs[self.active].stream;
        self.session.streams.get(&stream_id).expect("stream exists").show_extras
    }

    fn rename_active_tab(&mut self, name: String) {
        self.tabs[self.active].name = name;
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
        position: LogStreamPosition,
        display_time: chrono::DateTime<chrono::Utc>,
        display_msg: String,
    ) {
        let stream_id = self.tabs[self.active].stream;
        let bookmark = Bookmark {
            id: BookmarkId::new_v4(),
            created_at: chrono::Utc::now(),
            position,
            name,
            display_time,
            display_msg,
        };
        self.session.add_bookmark(stream_id, bookmark);
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
    }

    /// All user bookmarks, flattened across streams in the same order
    /// the Bookmarks tab renders them: streams in BTreeMap order, each
    /// stream's bucket in insertion order.  Used by selection-cursor
    /// movement and rendering.
    fn flat_bookmarks(&self) -> Vec<&Bookmark> {
        self.session.user_bookmarks.values().flat_map(|v| v.iter()).collect()
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
    /// new tab targeting that stream.  Resolves the bookmarked
    /// position against the active filter; on `FilteredOut` or `Gone`
    /// stashes a one-shot footer notice so the user knows what
    /// happened.
    fn navigate_to_bookmark_cursor(&mut self) {
        let Some(idx) = self.bookmark_cursor_idx() else {
            return;
        };
        let bookmarks = self.flat_bookmarks();
        let bm = bookmarks[idx];
        let target_stream = self
            .session
            .user_bookmarks
            .iter()
            .find(|(_, v)| v.iter().any(|b| b.id == bm.id))
            .map(|(s, _)| *s)
            .expect("bookmark belongs to some stream");
        let position = bm.position.clone();
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
        // Resolve the bookmark to a row in the now-active tab and
        // update the viewport.  resolve_position needs a borrow on the
        // engine, so unwrap the filter first.
        let filter =
            self.session.streams.get(&target_stream).unwrap().filter.clone();
        match self.engine.resolve_position(&filter, &position) {
            ResolvePosition::Found(event_idx) => {
                let line = self.tabs[tab_idx].first_line_for_event[event_idx];
                self.tabs[tab_idx].viewport_top = line;
            }
            ResolvePosition::FilteredOut(event_idx) => {
                // resolve_position guarantees the index is in range
                // when at least one event matches the filter; if the
                // filter strips everything the convention is `0`,
                // which is a valid line index for the empty viewport.
                let line = self.tabs[tab_idx]
                    .first_line_for_event
                    .get(event_idx)
                    .copied()
                    .unwrap_or(0);
                self.tabs[tab_idx].viewport_top = line;
                self.notice = Some(
                    "bookmarked entry is hidden by the active filter; \
                     jumped to the nearest visible entry"
                        .to_string(),
                );
            }
            ResolvePosition::Gone => {
                self.notice = Some(
                    "bookmarked entry is no longer present in any \
                     loaded source"
                        .to_string(),
                );
            }
        }
    }

    /// Installs `regex` as the active search on the current tab,
    /// records it as the most recent search at the app level, and
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
        let matches =
            compute_matches(&self.tabs[self.active].formatted, &regex);
        let tab = &mut self.tabs[self.active];
        tab.search =
            Some(TabSearch { pattern: pattern.clone(), regex, matches });
        self.last_search = Some(LastSearch { pattern, direction });
        self.jump_to_match(direction, /* exclusive = */ false);
    }

    /// Repeats the most recent search (used by `/<enter>` and
    /// `?<enter>` with an empty buffer).  Updates the stored direction
    /// so a follow-up `n` continues the way the user just chose.  No-op
    /// if there is no previous search.
    fn repeat_last_search(&mut self, direction: SearchDirection) {
        let pattern = match &self.last_search {
            Some(l) => l.pattern.clone(),
            None => return,
        };
        self.ensure_tab_search(&pattern);
        self.last_search = Some(LastSearch { pattern, direction });
        self.jump_to_match(direction, /* exclusive = */ true);
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
        self.jump_to_match(direction, /* exclusive = */ true);
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
        let matches = compute_matches(&tab.formatted, &regex);
        tab.search =
            Some(TabSearch { pattern: pattern.to_string(), regex, matches });
    }

    /// Move `viewport_top` to the next match in `direction`.  When
    /// `exclusive`, a match exactly at `viewport_top` is skipped (used
    /// for repeats — otherwise `/<enter>` would re-land on the current
    /// match forever).  Stays put if no further match exists.
    fn jump_to_match(&mut self, direction: SearchDirection, exclusive: bool) {
        let tab = &self.tabs[self.active];
        let Some(search) = &tab.search else {
            return;
        };
        let cur = tab.viewport_top;
        let target = match (direction, exclusive) {
            (SearchDirection::Forward, true) => {
                search.matches.iter().copied().find(|&m| m > cur)
            }
            (SearchDirection::Forward, false) => {
                search.matches.iter().copied().find(|&m| m >= cur)
            }
            (SearchDirection::Backward, true) => {
                search.matches.iter().rev().copied().find(|&m| m < cur)
            }
            (SearchDirection::Backward, false) => {
                search.matches.iter().rev().copied().find(|&m| m <= cur)
            }
        };
        if let Some(t) = target {
            self.tabs[self.active].viewport_top = t;
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
    fn advance_time(&mut self, forward: bool) {
        let mut delta = self.current_step_duration();
        if !forward {
            delta = -delta;
        }
        let h = self.viewport_height;
        self.active_tab_mut().advance_time(delta, h);
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
        self.tabs.remove(self.active);
        if self.tabs.is_empty() {
            self.push_tab(Filter::default());
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
    }

    /// Enters select mode on the active tab with the given action.
    /// The selection starts on the record at the top of the viewport
    /// (or the record whose extras the user is currently scrolled
    /// into).  No-op when the tab has no records.
    fn start_selection(&mut self, action: SelectionAction) {
        let tab = self.active_tab_mut();
        if tab.events.is_empty() {
            return;
        }
        // event_for_line is empty iff formatted is empty iff events is
        // empty (handled above).  Anything past the last line is
        // similarly impossible after the empty check, but be defensive
        // against a viewport_top set out of range by a future caller.
        let event_idx = tab
            .event_for_line
            .get(tab.viewport_top)
            .copied()
            .unwrap_or(tab.events.len() - 1);
        tab.select = Some(Selection { event_idx, action });
    }

    /// Routes a keystroke while the Bookmarks pane is active.
    ///
    /// Supported keys: j/k (move bookmark cursor), Enter (navigate to
    /// the bookmark — switches tabs or opens a new one), x (open the
    /// delete-confirmation dialog), Tab/BackTab (cycle panes), q/Esc/
    /// Ctrl-C (open the quit-confirmation dialog).  Everything else is
    /// dropped: filter edits, search, time-step navigation, and Ctrl-T/
    /// Ctrl-W make no sense in a list of bookmarks and would leave the
    /// user in a confusing state if half-handled.
    fn handle_bookmarks_key(&mut self, key: KeyEvent) {
        match key {
            KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
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
        let Some(Some(ee)) = tab.events.get(sel.event_idx) else {
            return;
        };
        match sel.action {
            SelectionAction::Exclude | SelectionAction::Include => {
                let negated = matches!(sel.action, SelectionAction::Exclude);
                let new_pred = Predicate::FieldEquals {
                    name: "msg".to_string(),
                    value: ee.event.msg.clone(),
                    negated,
                };
                let mut new_filter = self.active_filter().clone();
                new_filter.add_predicate(new_pred);
                // apply_filter resets viewport_top, search, and select.
                self.apply_filter(new_filter);
            }
            SelectionAction::Bookmark => {
                // Snapshot what the bookmark needs at creation time so
                // the Bookmarks-tab row can render even when the source
                // isn't currently loaded.
                let position = ee.position.clone();
                let display_time = ee.event.time;
                let display_msg = preview_msg(&ee.event.msg);
                self.dialog = Some(Dialog::bookmark_name(
                    position,
                    display_time,
                    display_msg,
                ));
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
            quit: false,
            dialog: None,
            last_search: None,
            time_step_idx: DEFAULT_TIME_STEP_IDX,
            bookmark_cursor: None,
            notice: None,
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
        let engine_events: Vec<Option<EngineEvent>> = events
            .into_iter()
            .map(|maybe| {
                maybe.map(|event| EngineEvent {
                    position: LogStreamPosition::new(
                        synthetic_source.clone(),
                        event.time,
                        0,
                    ),
                    event,
                })
            })
            .collect();
        let event_for_line: Vec<usize> = (0..formatted.len()).collect();
        let first_line_for_event: Vec<usize> =
            (0..engine_events.len()).collect();
        a.tabs.push(Tab {
            name: format!("Tab {}", a.next_tab_number),
            stream: stream_id,
            events: engine_events,
            formatted,
            event_for_line,
            first_line_for_event,
            viewport_top: 0,
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
                DialogResult::ApplyBookmark {
                    name,
                    position,
                    display_time,
                    display_msg,
                } => {
                    self.dialog = None;
                    self.add_bookmark(
                        name,
                        position,
                        display_time,
                        display_msg,
                    );
                }
                DialogResult::ApplyDeleteBookmark(id) => {
                    self.dialog = None;
                    self.delete_bookmark(id);
                }
                DialogResult::ApplyQuit => {
                    self.dialog = None;
                    self.quit = true;
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
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
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
                self.active_tab_mut().scroll_down(1, h);
            }
            KeyEvent {
                code: KeyCode::Char('k') | KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.active_tab_mut().scroll_up(1);
            }
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                let h = self.viewport_height;
                self.active_tab_mut().scroll_down(half_page, h);
            }
            KeyEvent {
                code: KeyCode::Char(' '),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let h = self.viewport_height;
                self.active_tab_mut().scroll_down(page, h);
            }
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.active_tab_mut().scroll_up(half_page);
            }
            KeyEvent {
                code: KeyCode::Char('g') | KeyCode::Home,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.active_tab_mut().viewport_top = 0;
            }
            // Different terminals report `G` with NONE or SHIFT; accept
            // both.  Don't accept CONTROL/ALT — those are unrelated
            // bindings the user might add later.
            KeyEvent { code: KeyCode::Char('G'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                let max = self.active_tab().max_top(self.viewport_height);
                self.active_tab_mut().viewport_top = max;
            }
            KeyEvent {
                code: KeyCode::End,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let max = self.active_tab().max_top(self.viewport_height);
                self.active_tab_mut().viewport_top = max;
            }
            KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.dialog = Some(Dialog::filter(self.active_filter()));
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
                self.dialog = Some(Dialog::search(SearchDirection::Forward));
            }
            KeyEvent { code: KeyCode::Char('?'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.dialog = Some(Dialog::search(SearchDirection::Backward));
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
                let cloned = self.active_filter().clone();
                self.push_tab(cloned);
                self.dialog = Some(Dialog::filter(self.active_filter()));
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
                self.advance_time(/* forward = */ true);
            }
            KeyEvent { code: KeyCode::Char('<'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.advance_time(/* forward = */ false);
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
            // Readline-style line editing.  ^U kills to BOL, ^W kills
            // the previous whitespace-delimited word (matching shell
            // behaviour, so a whole `name=Nexus` token disappears at
            // once), and Alt-B/Alt-F move by alphanumeric word so the
            // cursor can step inside a token like `level>=warn`.
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.kill_to_start();
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
    Search {
        editor: LineEditor,
        direction: SearchDirection,
        parse_error: Option<String>,
    },
    /// Naming a new bookmark just created via the `b` flow.  Carries
    /// the position (and its rendering preview) the bookmark will
    /// anchor to.  Empty buffer → unnamed bookmark.  No parse feedback;
    /// any string is a valid name.
    BookmarkName {
        editor: LineEditor,
        position: LogStreamPosition,
        display_time: chrono::DateTime<chrono::Utc>,
        display_msg: String,
    },
    /// Confirming the deletion of a bookmark from the Bookmarks tab.
    /// `id` identifies the bookmark; `label` is what to display in the
    /// dialog title.  No editor: the user picks Cancel (Esc) or
    /// Confirm (Enter).
    ConfirmDeleteBookmark { id: BookmarkId, label: String },
    /// Confirming a quit request triggered by `q`/`Esc`/`Ctrl-C` in
    /// the main or Bookmarks pane.  No editor: Esc cancels, Enter
    /// confirms.  Guards against accidental exits losing the user's
    /// in-flight filter, search, and viewport state.
    ConfirmQuit,
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
    ApplyBookmark {
        name: Option<BookmarkName>,
        position: LogStreamPosition,
        display_time: chrono::DateTime<chrono::Utc>,
        display_msg: String,
    },
    /// Close the dialog and delete the bookmark with this id.  If the
    /// id is no longer present (concurrent edits aren't a thing here,
    /// but defending against stale state is cheap) the action is a
    /// no-op at the App layer.
    ApplyDeleteBookmark(BookmarkId),
    /// Close the dialog and tear down the TUI: the user confirmed the
    /// quit prompt.
    ApplyQuit,
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

    fn search(direction: SearchDirection) -> Self {
        Self::Search {
            editor: LineEditor::new(String::new()),
            direction,
            parse_error: None,
        }
    }

    /// Builds the bookmark-name dialog for a freshly-selected row.
    /// Empty initial buffer means "unnamed by default"; the user types
    /// to add a name.
    fn bookmark_name(
        position: LogStreamPosition,
        display_time: chrono::DateTime<chrono::Utc>,
        display_msg: String,
    ) -> Self {
        Self::BookmarkName {
            editor: LineEditor::new(String::new()),
            position,
            display_time,
            display_msg,
        }
    }

    fn confirm_delete_bookmark(id: BookmarkId, label: String) -> Self {
        Self::ConfirmDeleteBookmark { id, label }
    }

    fn confirm_quit() -> Self {
        Self::ConfirmQuit
    }

    fn editor(&self) -> Option<&LineEditor> {
        match self {
            Self::Filter { editor, .. }
            | Self::Rename { editor }
            | Self::Search { editor, .. }
            | Self::BookmarkName { editor, .. } => Some(editor),
            Self::ConfirmDeleteBookmark { .. } | Self::ConfirmQuit => None,
        }
    }

    fn parse_error(&self) -> Option<&str> {
        match self {
            Self::Filter { parse_error, .. }
            | Self::Search { parse_error, .. } => parse_error.as_deref(),
            Self::Rename { .. }
            | Self::BookmarkName { .. }
            | Self::ConfirmDeleteBookmark { .. }
            | Self::ConfirmQuit => None,
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
        let editor_result = match self {
            Self::Filter { editor, .. }
            | Self::Rename { editor }
            | Self::Search { editor, .. }
            | Self::BookmarkName { editor, .. } => editor.handle_edit(key),
            // Confirmation dialogs have no editor; non-Esc/Enter keys
            // are dropped on the floor so a stray `j`/`q` doesn't fall
            // through to the underlying tab.
            Self::ConfirmDeleteBookmark { .. } | Self::ConfirmQuit => {
                return DialogResult::Stay;
            }
        };
        if let EditAction::Handled = editor_result {
            self.reparse_filter();
            self.reparse_search();
        }
        DialogResult::Stay
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
            Self::Search { editor, direction, parse_error } => {
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
            Self::BookmarkName {
                editor,
                position,
                display_time,
                display_msg,
            } => {
                let trimmed = editor.text.trim();
                let name = if trimmed.is_empty() {
                    None
                } else {
                    Some(BookmarkName::from(trimmed.to_string()))
                };
                DialogResult::ApplyBookmark {
                    name,
                    position: position.clone(),
                    display_time: *display_time,
                    display_msg: display_msg.clone(),
                }
            }
            Self::ConfirmDeleteBookmark { id, .. } => {
                DialogResult::ApplyDeleteBookmark(*id)
            }
            Self::ConfirmQuit => DialogResult::ApplyQuit,
        }
    }
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
    let [tabs_area, content_area, bottom_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(bottom_height),
    ])
    .areas(area);

    render_tab_bar(frame, app, tabs_area);

    app.viewport_height = content_area.height;

    if app.bookmarks_active() {
        render_bookmarks_pane(frame, app, content_area);
        render_bookmarks_footer(frame, app, bottom_area);
        if let Some(
            d @ (Dialog::ConfirmDeleteBookmark { .. } | Dialog::ConfirmQuit),
        ) = app.dialog.as_ref()
        {
            render_dialog(frame, d, area);
        }
        return;
    }

    // Re-clamp in case the viewport just shrank past the previous top.
    let max_top = app.active_tab().max_top(app.viewport_height);
    if app.active_tab().viewport_top > max_top {
        app.active_tab_mut().viewport_top = max_top;
    }

    let tab = app.active_tab();
    let total = tab.formatted.len();
    let top = tab.viewport_top;
    let bottom = (top + content_area.height as usize).min(total);

    let selected_event = tab.select.map(|s| s.event_idx);
    let lines: Vec<Line<'_>> = tab.formatted[top..bottom]
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let line_index = top + i;
            let mut line = match &tab.search {
                Some(search) => highlight_line(s, &search.regex),
                None => Line::raw(s.as_str()),
            };
            // Highlight every display line that belongs to the
            // selected record so users see the full record they're
            // about to exclude/include/bookmark, not just its header
            // row.  Distinct from the search highlight (REVERSED on
            // matched runs); a row-wide background reads as "this is
            // the entry you're about to act on" without fighting
            // search styling.
            let selected = selected_event.is_some_and(|target| {
                tab.event_for_line.get(line_index).copied() == Some(target)
            });
            if selected {
                line = line.style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
            }
            line
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), content_area);

    // Bottom strip: search prompt or footer, never both.
    match app.dialog.as_ref() {
        Some(Dialog::Search { editor, direction, parse_error }) => {
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
                let entry_total = tab.events.len();
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
            } else if total == 0 {
                format!(
                    "q quit · f filter · F fields={} · / search · \
                     </> step={} · x/X exclude/include · b bookmark · \
                     ^T new · ^W close · r rename · 0/0",
                    if app.active_show_extras() { "on" } else { "off" },
                    app.current_step_label(),
                )
            } else {
                format!(
                    "q quit · f filter · F fields={} · / search · \
                     </> step={} · x/X exclude/include · b bookmark · \
                     ^T new · ^W close · r rename · {}-{} of {}",
                    if app.active_show_extras() { "on" } else { "off" },
                    app.current_step_label(),
                    top + 1,
                    bottom,
                    total,
                )
            };
            frame.render_widget(Paragraph::new(footer), bottom_area);
        }
    }

    // Centered popups (Filter, Rename, BookmarkName,
    // ConfirmDeleteBookmark, ConfirmQuit) draw on top of the rest.
    // The Search prompt is laid out inline above and is skipped here.
    if let Some(
        dialog @ (Dialog::Filter { .. }
        | Dialog::Rename { .. }
        | Dialog::BookmarkName { .. }
        | Dialog::ConfirmDeleteBookmark { .. }
        | Dialog::ConfirmQuit),
    ) = app.dialog.as_ref()
    {
        render_dialog(frame, dialog, area);
    }
}

/// Renders the Bookmarks pane: one row per bookmark, with the cursor
/// highlight on the selected row.  Each row is `created · file · time
/// · msg-snippet`.  The Bookmarks pane has no scrolling yet (matching
/// the existing tab content area, which doesn't either when smaller
/// than viewport); rows below the viewport simply don't render until
/// we add scrolling.
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
    let lines: Vec<Line<'_>> = bookmarks
        .iter()
        .take(area.height as usize)
        .map(|bm| {
            let source_str: &str = bm.position.source().as_ref();
            let basename = std::path::Path::new(source_str)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(source_str)
                .to_string();
            let name =
                bm.name.as_ref().map(|n| format!(" [{n}]")).unwrap_or_default();
            let row = format!(
                "{} · {} · {} · {}{}",
                bm.created_at.to_rfc3339(),
                basename,
                bm.display_time.to_rfc3339(),
                bm.display_msg,
                name,
            );
            let mut line = Line::raw(row);
            if Some(bm.id) == cursor_id {
                line = line.style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
            }
            line
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_bookmarks_footer(frame: &mut Frame, app: &App, area: Rect) {
    let count = app.session.bookmark_count();
    let footer = if let Some(notice) = app.notice.as_deref() {
        notice.to_string()
    } else {
        format!(
            "q quit · j/k select · Enter open · x delete · \
             Tab cycle · {count} bookmark{}",
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

/// Carves a centered popup over `area` and draws either dialog variant.
///
/// Variants with an editor (Filter/Rename/Search/BookmarkName) render
/// their text and cursor on the first row; Filter/Search additionally
/// render any parse error in red below.  ConfirmDeleteBookmark and
/// ConfirmQuit have no editor and show only the question encoded in
/// their title.
fn render_dialog(frame: &mut Frame, dialog: &Dialog, area: Rect) {
    let popup = popup_area(area, 70, 5);
    // Clear the underlying rows so the editor isn't drawn on top of
    // them.
    frame.render_widget(Clear, popup);

    let block = Block::bordered().title(dialog.title());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [edit_area, error_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)])
            .areas(inner);

    if let Some(editor) = dialog.editor() {
        frame.render_widget(
            Paragraph::new(Line::raw(editor.text.as_str())),
            edit_area,
        );
        // Cursor column: the dialog buffers are ASCII in practice
        // (filter syntax is ASCII, tab names and bookmark names
        // typically too), so the byte offset doubles as the column.
        // If we ever accept multibyte chars we'd need to compute the
        // display width here instead.
        let col = edit_area
            .x
            .saturating_add(u16::try_from(editor.cursor).unwrap_or(u16::MAX));
        let col = col.min(edit_area.x.saturating_add(edit_area.width));
        frame.set_cursor_position(Position::new(col, edit_area.y));
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
    fn esc_opens_quit_confirmation() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Esc));
        assert!(!a.quit);
        assert!(matches!(a.dialog, Some(Dialog::ConfirmQuit)));
    }

    #[test]
    fn ctrl_c_opens_quit_confirmation() {
        let mut a = app(10, 5);
        a.handle_key(ctrl('c'));
        assert!(!a.quit);
        assert!(matches!(a.dialog, Some(Dialog::ConfirmQuit)));
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
        a.active_tab_mut().viewport_top = 3;
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

    #[test]
    fn ctrl_u_scrolls_half_page_up() {
        let mut a = app(100, 10);
        a.active_tab_mut().viewport_top = 20;
        a.handle_key(ctrl('u'));
        assert_eq!(a.active_tab().viewport_top, 15);
    }

    #[test]
    fn g_jumps_top() {
        let mut a = app(100, 10);
        a.active_tab_mut().viewport_top = 50;
        a.handle_key(key(KeyCode::Char('g')));
        assert_eq!(a.active_tab().viewport_top, 0);
    }

    #[test]
    fn home_jumps_top() {
        let mut a = app(100, 10);
        a.active_tab_mut().viewport_top = 50;
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
        a.active_tab_mut().viewport_top = 5; // == max_top
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
        // Wide enough to hold the entire footer including the trailing
        // dynamic info (step indicator, fields toggle, "1-3 of 3"
        // counter) without truncation; the live footer is sized to be
        // legible at 80 cols even when terminals truncate it.
        let backend = TestBackend::new(160, 6);
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
        assert!(dump.contains("1-3 of 3"), "dump:\n{dump}");
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
    fn dialog_typing_inserts_at_cursor() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "name=Nexus");
        assert_eq!(d.editor().unwrap().text, "name=Nexus");
        assert_eq!(d.editor().unwrap().cursor, "name=Nexus".len());
    }

    #[test]
    fn dialog_backspace_deletes_char_before_cursor() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Backspace));
        assert_eq!(d.editor().unwrap().text, "ab");
        assert_eq!(d.editor().unwrap().cursor, 2);
    }

    #[test]
    fn dialog_left_right_move_cursor() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Left));
        assert_eq!(d.editor().unwrap().cursor, 2);
        d.handle_key(key(KeyCode::Home));
        assert_eq!(d.editor().unwrap().cursor, 0);
        d.handle_key(key(KeyCode::Right));
        assert_eq!(d.editor().unwrap().cursor, 1);
        d.handle_key(key(KeyCode::End));
        assert_eq!(d.editor().unwrap().cursor, 3);
    }

    #[test]
    fn dialog_delete_removes_char_after_cursor() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(key(KeyCode::Delete));
        assert_eq!(d.editor().unwrap().text, "bc");
        assert_eq!(d.editor().unwrap().cursor, 0);
    }

    #[test]
    fn dialog_ctrl_u_kills_to_start_of_line() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        // Position cursor inside "Nexus".
        for _ in 0..3 {
            d.handle_key(key(KeyCode::Left));
        }
        let cursor_before = d.editor().unwrap().cursor;
        d.handle_key(ctrl('u'));
        assert_eq!(d.editor().unwrap().text, "xus");
        assert_eq!(d.editor().unwrap().cursor, 0);
        assert!(cursor_before > 0);
    }

    #[test]
    fn dialog_ctrl_u_at_start_is_noop() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(ctrl('u'));
        assert_eq!(d.editor().unwrap().text, "abc");
        assert_eq!(d.editor().unwrap().cursor, 0);
    }

    #[test]
    fn dialog_ctrl_w_kills_previous_whitespace_word() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        d.handle_key(ctrl('w'));
        // The whole `name=Nexus` token disappears, plus the space.
        assert_eq!(d.editor().unwrap().text, "level>=warn ");
        assert_eq!(d.editor().unwrap().cursor, "level>=warn ".len());
    }

    #[test]
    fn dialog_ctrl_w_consumes_trailing_whitespace_first() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "name=Nexus   ");
        d.handle_key(ctrl('w'));
        assert_eq!(d.editor().unwrap().text, "");
        assert_eq!(d.editor().unwrap().cursor, 0);
    }

    #[test]
    fn dialog_alt_b_moves_back_one_alphanumeric_word() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        d.handle_key(alt('b'));
        assert_eq!(
            &d.editor().unwrap().text[d.editor().unwrap().cursor..],
            "Nexus"
        );
        d.handle_key(alt('b'));
        assert_eq!(
            &d.editor().unwrap().text[d.editor().unwrap().cursor..],
            "name=Nexus"
        );
        d.handle_key(alt('b'));
        assert_eq!(
            &d.editor().unwrap().text[d.editor().unwrap().cursor..],
            "warn name=Nexus",
        );
        d.handle_key(alt('b'));
        assert_eq!(d.editor().unwrap().cursor, 0);
        // Once more: clamped at zero.
        d.handle_key(alt('b'));
        assert_eq!(d.editor().unwrap().cursor, 0);
    }

    #[test]
    fn dialog_alt_f_moves_forward_one_alphanumeric_word() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(alt('f'));
        assert_eq!(
            &d.editor().unwrap().text[..d.editor().unwrap().cursor],
            "level"
        );
        d.handle_key(alt('f'));
        assert_eq!(
            &d.editor().unwrap().text[..d.editor().unwrap().cursor],
            "level>=warn"
        );
        d.handle_key(alt('f'));
        assert_eq!(
            &d.editor().unwrap().text[..d.editor().unwrap().cursor],
            "level>=warn name",
        );
        d.handle_key(alt('f'));
        assert_eq!(d.editor().unwrap().cursor, d.editor().unwrap().text.len());
        // Once more: clamped.
        d.handle_key(alt('f'));
        assert_eq!(d.editor().unwrap().cursor, d.editor().unwrap().text.len());
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
        assert_eq!(a.active_filter().to_string(), "level>=warn");
    }

    #[test]
    fn dialog_escape_discards_changes() {
        let mut a = app(10, 5);
        let original_filter = a.active_filter().to_string();
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "name=Nexus");
        a.handle_key(key(KeyCode::Esc));
        assert!(a.dialog.is_none());
        assert_eq!(a.active_filter().to_string(), original_filter);
    }

    #[test]
    fn dialog_apply_resets_viewport_and_requeries_engine() {
        // Build a real engine with a tiny bunyan file so apply_filter
        // can re-run query_events.  This is the only filter-dialog
        // test that isn't pure state-machine.
        use camino_tempfile::tempdir;
        use slog::{Drain, Logger, info, o, warn};
        use std::fs::OpenOptions;
        use std::sync::Mutex;

        let dir = tempdir().unwrap();
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
        let mut a = App::new(engine);
        a.viewport_height = 2;
        a.active_tab_mut().viewport_top = 3;
        assert_eq!(a.active_tab().formatted.len(), 6);

        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "level>=warn");
        a.handle_key(key(KeyCode::Enter));

        assert!(a.dialog.is_none());
        assert_eq!(a.active_tab().formatted.len(), 1);
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
        assert_eq!(a.active_filter().to_string(), "level>=warn");
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
        assert_eq!(a.active_filter().to_string(), "level>=warn");
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
        assert!(a.active_filter().predicates().is_empty());
    }

    #[test]
    fn each_tab_keeps_its_own_viewport_top() {
        let mut a = app(100, 10);
        a.active_tab_mut().viewport_top = 30;
        a.handle_key(ctrl('t'));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.active_tab().viewport_top, 0);
        a.active_tab_mut().viewport_top = 50;
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
    fn search_dialog_typing_inserts() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha");
        assert_eq!(a.dialog.as_ref().unwrap().editor().unwrap().text, "alpha");
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
        a.active_tab_mut().viewport_top = 3;
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
        a.active_tab_mut().viewport_top = 5;
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
    fn slash_enter_repeats_last_search_forward() {
        let mut a = search_app();
        // Initial backward search.
        a.active_tab_mut().viewport_top = 5;
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
        a.active_tab_mut().formatted = (0..10)
            .map(|i| {
                if i % 2 == 0 {
                    format!("alpha row {i}")
                } else {
                    format!("beta row {i}")
                }
            })
            .collect();
        a.active_tab_mut().search = None;
        a.active_tab_mut().viewport_top = 3;

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
        Selection { event_idx, action: SelectionAction::Exclude }
    }

    fn incl_sel(event_idx: usize) -> Selection {
        Selection { event_idx, action: SelectionAction::Include }
    }

    fn bm_sel(event_idx: usize) -> Selection {
        Selection { event_idx, action: SelectionAction::Bookmark }
    }

    #[test]
    fn x_enters_exclude_mode_at_viewport_top() {
        let mut a = select_app(10, 5);
        a.active_tab_mut().viewport_top = 3;
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
        a.active_tab_mut().viewport_top = 3;
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
        a.active_tab_mut().viewport_top = 5;
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
        let before = a.active_filter().to_string();
        a.handle_key(key(KeyCode::Char('x')));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.active_tab().select, None);
        assert_eq!(a.active_filter().to_string(), before);
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
        let displayed = a.active_filter().to_string();
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
        let displayed = a.active_filter().to_string();
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
        let before = a.active_filter().to_string();
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.active_tab().select, Some(excl_sel(1)));
        assert_eq!(a.active_filter().to_string(), before);
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
        // Tall enough to fit the tab bar, two content rows, and a
        // footer.  Selection is on row 1 (the second event).
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut a = select_app(3, 2);
        a.handle_key(key(KeyCode::Char('x')));
        a.handle_key(key(KeyCode::Char('j')));
        terminal.draw(|frame| render(frame, &mut a)).unwrap();
        let buf = terminal.backend().buffer();
        // Layout: row 0 = tab bar, row 1 = first content row, row 2 =
        // second content row (the selected one), row 3 = footer.
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
        let max = a.active_tab().max_top(a.viewport_height);
        assert_eq!(a.active_tab().viewport_top, max);
    }

    #[test]
    fn gt_snaps_to_end_when_no_event_past_target() {
        // 60 events at 1s; +1m from row 0 has no event at >= t=60.
        let mut a = time_app(60, 10);
        a.handle_key(shift('>'));
        let max = a.active_tab().max_top(a.viewport_height);
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
        a.active_tab_mut().viewport_top = 90;
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
        a.active_tab_mut().viewport_top = 1;
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
        a.active_tab_mut().viewport_top = 1;
        a.handle_key(shift('>'));
        let max = a.active_tab().max_top(a.viewport_height);
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
        a.active_tab_mut().viewport_top = 3;
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
        assert_eq!(
            bms[0].name.as_ref().map(|n| n.to_string()),
            Some("first".to_string()),
        );
        assert!(bms[1].name.is_none());
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

    fn multi_line_app(
        records: &[RecordSpec<'_>],
    ) -> (App, camino_tempfile::Utf8TempDir) {
        use camino_tempfile::tempdir;
        use std::io::Write;
        let dir = tempdir().unwrap();
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
        let mut a = App::new(engine);
        // The multi-line tests are about exactly the line/event split
        // that extras introduce.  Streams hide extras by default, so
        // flip the toggle on before handing the app back.
        a.toggle_show_extras();
        a.viewport_height = 10;
        (a, dir)
    }

    #[test]
    fn render_emits_multiple_lines_per_event_with_extras() {
        let (a, _dir) = multi_line_app(&[
            (10, "starting", &[("build", r#""0.1.0""#)]),
            (20, "tick", &[]),
            (30, "loaded", &[("zones", "4"), ("ms", "12")]),
        ]);
        let tab = a.active_tab();
        // 3 events, but 6 display lines: 2 + 1 + 3.
        assert_eq!(tab.events.len(), 3);
        assert_eq!(tab.formatted.len(), 6);
        assert_eq!(tab.first_line_for_event, vec![0, 2, 3]);
        assert_eq!(tab.event_for_line, vec![0, 0, 1, 2, 2, 2]);
        // Spot-check the indented-extras layout.
        assert!(tab.formatted[0].ends_with(": starting"));
        assert_eq!(tab.formatted[1], r#"    build = "0.1.0""#);
        assert!(tab.formatted[2].ends_with(": tick"));
        assert!(tab.formatted[3].ends_with(": loaded"));
        assert_eq!(tab.formatted[4], "    ms = 12");
        assert_eq!(tab.formatted[5], "    zones = 4");
    }

    #[test]
    fn select_j_moves_to_next_event_skipping_extra_lines() {
        // First event has two extras (3 lines total); second has none
        // (1 line).  A single `j` in select mode must land on the
        // *second event*, not on one of the first event's extra rows.
        let (mut a, _dir) = multi_line_app(&[
            (10, "first", &[("a", "1"), ("b", "2")]),
            (20, "second", &[]),
        ]);
        a.handle_key(key(KeyCode::Char('x')));
        assert_eq!(a.active_tab().select.unwrap().event_idx, 0);
        a.handle_key(key(KeyCode::Char('j')));
        assert_eq!(a.active_tab().select.unwrap().event_idx, 1);
    }

    #[test]
    fn render_highlights_all_lines_of_selected_event() {
        // Event 1 spans lines 1, 2, 3 (header + 2 extras).  Selecting
        // it must paint the dark-gray bg on every one of those rows.
        let (mut a, _dir) = multi_line_app(&[
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
        let (mut a, _dir) = multi_line_app(&[
            (10, "first", &[("k", "1")]), // lines 0, 1
            (80, "later", &[]),           // line 2 — 70s after first
            (90, "filler", &[]),          // line 3
            (100, "filler", &[]),         // line 4
            (110, "filler", &[]),         // line 5
        ]);
        a.viewport_height = 2;
        a.active_tab_mut().viewport_top = 1; // an extras row of event 0
        a.handle_key(shift('>'));
        // step is 1m; from t=10 + 60s = 70 → next event at t=80 wins.
        // Its first display line is line 2; max_top with 6 lines and
        // height 2 is 4, so the result is not clamped.
        assert_eq!(a.active_tab().viewport_top, 2);
    }

    #[test]
    fn footer_reports_entry_count_in_select_mode() {
        // 3 events but 6 display lines: footer in select mode should
        // say "entry 1/3", not "row 1/6".
        let (mut a, _dir) = multi_line_app(&[
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
    }

    // ---------- show_extras toggle (F) ----------

    #[test]
    fn streams_default_to_hiding_extras() {
        // A fresh stream produced by `push_tab` must hide structured
        // extras: the multi-line file below has two events, each with
        // its own extras, but the rendered tab should be just the two
        // header lines.
        use camino_tempfile::tempdir;
        use std::io::Write;
        let dir = tempdir().unwrap();
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
        let a = App::new(engine);
        let tab = a.active_tab();
        assert_eq!(tab.events.len(), 2);
        assert_eq!(tab.formatted.len(), 2, "extras should be hidden");
        assert!(tab.formatted[0].ends_with(": first"));
        assert!(tab.formatted[1].ends_with(": second"));
        assert!(!a.active_show_extras());
    }

    #[test]
    fn shift_f_toggles_show_extras_and_repaints() {
        let (mut a, _dir) = multi_line_app(&[
            (10, "first", &[("build", r#""0.1.0""#)]),
            (20, "tick", &[]),
        ]);
        // multi_line_app already enabled extras for its own tests; flip
        // it off, then back on, asserting the line count tracks the
        // setting and that `F` is the user-visible binding.
        assert!(a.active_show_extras());
        assert_eq!(a.active_tab().formatted.len(), 3);
        a.handle_key(shift('F'));
        assert!(!a.active_show_extras());
        assert_eq!(a.active_tab().formatted.len(), 2);
        // Bare `F` (some terminals don't set the SHIFT modifier) toggles
        // back on.
        a.handle_key(key(KeyCode::Char('F')));
        assert!(a.active_show_extras());
        assert_eq!(a.active_tab().formatted.len(), 3);
    }

    #[test]
    fn show_extras_toggle_preserves_anchor_record() {
        // Three records, the second has two extras.  Park the viewport
        // on the second event's header (line 2 with extras showing) and
        // toggle off — viewport should snap to the same record at its
        // new (post-rerender) line.
        let (mut a, _dir) = multi_line_app(&[
            (10, "first", &[]),
            (20, "second", &[("a", "1"), ("b", "2")]),
            (30, "third", &[]),
        ]);
        // With extras: lines = [first, second, a-row, b-row, third].
        a.active_tab_mut().viewport_top = 1; // second event's header
        a.handle_key(shift('F')); // hide
        // Without extras: lines = [first, second, third].  The same
        // record's first line is now index 1.
        assert_eq!(a.active_tab().viewport_top, 1);
        assert_eq!(a.active_tab().formatted.len(), 3);
        a.handle_key(shift('F')); // show again
        // First line for record 1 is still index 1 (event 0 is single
        // line).  Anchor preserved across the second toggle too.
        assert_eq!(a.active_tab().viewport_top, 1);
    }

    #[test]
    fn show_extras_persists_into_session_round_trip() {
        // Toggling extras on must survive a session save/load cycle:
        // the user shouldn't have to flip F again every time they
        // re-open the project.
        let (mut a, _dir) = multi_line_app(&[(10, "first", &[("k", "1")])]);
        // multi_line_app starts with extras enabled; flip off so we
        // exercise a non-default value through the round-trip.
        a.handle_key(shift('F'));
        assert!(!a.active_show_extras());
        let json = serde_json::to_string(&a.session).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        let stream_id = a.tabs[a.active].stream;
        let stream = restored.streams.get(&stream_id).unwrap();
        assert!(!stream.show_extras);
    }
}
