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
use seer::{Engine, Event as LogEvent, Filter, Predicate};
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

    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;
    let mut app = App::new(engine);
    while !app.quit {
        terminal.draw(|frame| render(frame, &mut app))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(key);
        }
    }
    Ok(())
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
/// underlying events alongside their pre-formatted display strings.  The
/// two vecs are the same length and indexed identically; an `events`
/// entry is `None` exactly where the corresponding row was a parse/I/O
/// error.  Keeping the events lets callers (e.g. exclude mode) build
/// new filter predicates from the row under the cursor.
fn render_rows(
    engine: &Engine,
    filter: &Filter,
) -> (Vec<Option<LogEvent>>, Vec<String>) {
    let mut events = Vec::new();
    let mut formatted = Vec::new();
    for r in engine.query_events(filter) {
        match r {
            Ok(e) => {
                formatted.push(format_event(&e));
                events.push(Some(e));
            }
            Err(err) => {
                // SourceError's Display already says "I/O error: ...",
                // "failed to parse ...", or "warning: ..." as
                // appropriate; don't add another prefix.
                formatted.push(err.to_string());
                events.push(None);
            }
        }
    }
    (events, formatted)
}

fn format_event(e: &LogEvent) -> String {
    format!(
        "{} [{}] {}/{}/{}: {}",
        e.time.to_rfc3339(),
        e.level,
        e.name,
        e.hostname,
        e.pid,
        e.msg,
    )
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

/// One independent view: name, filter, the rows produced by querying
/// the engine with that filter, and the scroll offset within those
/// rows.
///
/// `events` and `formatted` have identical length and are indexed the
/// same way.  `events[i]` is `None` exactly when that row came from a
/// parse or I/O error rather than a real log record; the stringified
/// error sits in `formatted[i]`.  Keeping the parsed events around lets
/// in-place actions (e.g. exclude mode's "filter out entries like this")
/// inspect the record under the cursor without re-parsing.
struct Tab {
    name: String,
    filter: Filter,
    events: Vec<Option<LogEvent>>,
    formatted: Vec<String>,
    /// Index of the row at the top of the viewport.
    viewport_top: usize,
    /// Active highlighted search, if any.  Cleared when `formatted` is
    /// re-queried (filter change), because match indices would
    /// otherwise dangle.
    search: Option<TabSearch>,
    /// When `Some`, select mode is active.  The contained value carries
    /// the row index currently highlighted and the polarity (`x` →
    /// exclude, `X` → include) used to build the predicate on commit.
    /// Cleared whenever `formatted` is re-queried so the index can't
    /// dangle.
    select: Option<Selection>,
}

/// State of an in-progress `x`/`X` selection.
///
/// `row` is an absolute index into [`Tab::formatted`].  `negated` is
/// the polarity that will be baked into the predicate when the user
/// hits Enter — `true` for `x` (exclude: `msg!=value`), `false` for
/// `X` (include: `msg=value`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    row: usize,
    negated: bool,
}

impl Tab {
    fn new(name: String, engine: &Engine, filter: Filter) -> Self {
        let (events, formatted) = render_rows(engine, &filter);
        Self {
            name,
            filter,
            events,
            formatted,
            viewport_top: 0,
            search: None,
            select: None,
        }
    }

    fn apply_filter(&mut self, engine: &Engine, filter: Filter) {
        let (events, formatted) = render_rows(engine, &filter);
        self.events = events;
        self.formatted = formatted;
        self.filter = filter;
        self.viewport_top = 0;
        self.search = None;
        self.select = None;
    }

    /// Largest valid `viewport_top`: the row index that places the last
    /// row of `formatted` flush with the bottom of the viewport.
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

    /// Moves the select-mode highlight by `delta` rows (positive ==
    /// down) and scrolls the viewport just enough to keep the new
    /// selection visible.  No-op if select mode is not active or if
    /// there are no rows.
    fn move_selection(&mut self, delta: isize, viewport_height: u16) {
        let Some(sel) = self.select else {
            return;
        };
        if self.formatted.is_empty() {
            return;
        }
        let last = self.formatted.len() - 1;
        let new_row =
            (sel.row as isize + delta).clamp(0, last as isize) as usize;
        self.select = Some(Selection { row: new_row, ..sel });
        // Re-anchor the viewport.  `viewport_height` is sometimes 0 in
        // tests that don't render — saturating math keeps that case
        // sensible (selection visible at viewport_top).
        let h = viewport_height as usize;
        if new_row < self.viewport_top {
            self.viewport_top = new_row;
        } else if h > 0 && new_row >= self.viewport_top + h {
            self.viewport_top = new_row + 1 - h;
        }
    }

    /// Index of the closest event to `viewport_top` in the requested
    /// direction.  Falls back to the opposite direction so a viewport
    /// parked on an error row at one end of the file still gets an
    /// anchor; returns `None` only when there are no parsed events at
    /// all.  Used by [`Self::advance_time`] to decide what timestamp
    /// to add the step to.
    fn time_anchor_idx(&self, prefer_forward: bool) -> Option<usize> {
        let forward = self
            .events
            .iter()
            .enumerate()
            .skip(self.viewport_top)
            .find_map(|(i, e)| e.as_ref().map(|_| i));
        let backward = self
            .events
            .iter()
            .enumerate()
            .take(self.viewport_top.saturating_add(1))
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
            .time;
        let target = anchor_time + delta;
        let new_top = if go_forward {
            self.events
                .iter()
                .enumerate()
                .skip(anchor_idx)
                .find_map(|(i, e)| {
                    e.as_ref().filter(|ev| ev.time >= target).map(|_| i)
                })
                .unwrap_or_else(|| self.max_top(viewport_height))
        } else {
            self.events
                .iter()
                .enumerate()
                .take(anchor_idx + 1)
                .rev()
                .find_map(|(i, e)| {
                    e.as_ref().filter(|ev| ev.time <= target).map(|_| i)
                })
                .unwrap_or(0)
        };
        let max = self.max_top(viewport_height);
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
struct App {
    engine: Engine,
    /// Open tabs, in display order.  Invariant: never empty (closing
    /// the last tab pushes a fresh one to maintain this).
    tabs: Vec<Tab>,
    /// Index into `tabs` of the currently visible one.
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
}

impl App {
    fn new(engine: Engine) -> Self {
        let mut a = Self {
            engine,
            tabs: Vec::new(),
            active: 0,
            next_tab_number: 1,
            viewport_height: 0,
            quit: false,
            dialog: None,
            last_search: None,
            time_step_idx: DEFAULT_TIME_STEP_IDX,
        };
        a.push_tab(Filter::default());
        a
    }

    /// Pushes a new tab with the given filter and switches focus to
    /// it.  Does *not* open the filter dialog — callers that want that
    /// (e.g. Ctrl-T) do it explicitly after.
    fn push_tab(&mut self, filter: Filter) {
        let name = format!("Tab {}", self.next_tab_number);
        self.next_tab_number += 1;
        let tab = Tab::new(name, &self.engine, filter);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Replaces the active tab's filter, re-queries the engine, and
    /// resets that tab's viewport to the top.  Other tabs are
    /// untouched.
    fn apply_filter(&mut self, filter: Filter) {
        let engine = &self.engine;
        self.tabs[self.active].apply_filter(engine, filter);
    }

    fn rename_active_tab(&mut self, name: String) {
        self.tabs[self.active].name = name;
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
        tab.search = Some(TabSearch {
            pattern: pattern.clone(),
            regex,
            matches,
        });
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
        tab.search = Some(TabSearch {
            pattern: pattern.to_string(),
            regex,
            matches,
        });
    }

    /// Move `viewport_top` to the next match in `direction`.  When
    /// `exclusive`, a match exactly at `viewport_top` is skipped (used
    /// for repeats — otherwise `/<enter>` would re-land on the current
    /// match forever).  Stays put if no further match exists.
    fn jump_to_match(
        &mut self,
        direction: SearchDirection,
        exclusive: bool,
    ) {
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
        self.active = (self.active + 1) % self.tabs.len();
    }

    fn prev_tab(&mut self) {
        self.active =
            (self.active + self.tabs.len() - 1) % self.tabs.len();
    }

    /// Removes the active tab.  When the last tab is closed, a fresh
    /// default tab is created so the `!tabs.is_empty()` invariant
    /// holds.
    fn close_active_tab(&mut self) {
        self.tabs.remove(self.active);
        if self.tabs.is_empty() {
            self.push_tab(Filter::default());
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
    }

    /// Enters select mode on the active tab with the given polarity.
    /// `negated` is `true` for `x` (exclude on commit) and `false` for
    /// `X` (include on commit).  No-op when the tab has no rows.
    fn start_selection(&mut self, negated: bool) {
        let tab = self.active_tab_mut();
        if tab.formatted.is_empty() {
            return;
        }
        tab.select = Some(Selection { row: tab.viewport_top, negated });
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

    /// Build a `msg=<selected msg>` (or `msg!=<selected msg>`,
    /// depending on the selection's polarity) predicate from the row
    /// under the highlight, append it to the active filter, and
    /// re-query.  When the selected row is an error (no parsed event)
    /// this is a no-op: there's no `msg` to extract, and silently
    /// exiting the mode would be more confusing than just doing nothing
    /// and letting the user pick a different row or Esc out.
    fn commit_selection(&mut self) {
        let tab = &self.tabs[self.active];
        let Some(sel) = tab.select else {
            return;
        };
        let Some(Some(event)) = tab.events.get(sel.row) else {
            return;
        };
        let new_pred = Predicate::FieldEquals {
            name: "msg".to_string(),
            value: event.msg.clone(),
            negated: sel.negated,
        };
        let mut new_filter = tab.filter.clone();
        new_filter.add_predicate(new_pred);
        // apply_filter resets viewport_top, search, and select.
        self.apply_filter(new_filter);
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
    #[cfg(test)]
    fn with_events(
        events: Vec<Option<LogEvent>>,
        formatted: Vec<String>,
    ) -> Self {
        assert_eq!(events.len(), formatted.len());
        let mut a = Self {
            engine: Engine::new(),
            tabs: Vec::new(),
            active: 0,
            // The first push_tab below consumes "Tab 1".
            next_tab_number: 1,
            viewport_height: 0,
            quit: false,
            dialog: None,
            last_search: None,
            time_step_idx: DEFAULT_TIME_STEP_IDX,
        };
        // Manually push so we can override the row data (the engine
        // has no sources, so a real push_tab would yield empty vecs).
        a.tabs.push(Tab {
            name: format!("Tab {}", a.next_tab_number),
            filter: Filter::default(),
            events,
            formatted,
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
            }
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
                self.quit = true;
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
            KeyEvent {
                code: KeyCode::Char('G'), modifiers, ..
            } if modifiers == KeyModifiers::NONE
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
                self.dialog =
                    Some(Dialog::filter(&self.active_tab().filter));
            }
            // `x`: enter select mode for *exclusion*; `X`: same mode
            // but for *inclusion*.  Different terminals report `X` with
            // either NONE or SHIFT modifiers (matching `G`/`?`), so we
            // accept both.  The selection starts at `viewport_top` so
            // the user can immediately move it through the visible
            // rows; if the tab is empty there's nothing to select
            // against, so this is a no-op.
            KeyEvent {
                code: KeyCode::Char('x'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.start_selection(/* negated = */ true);
            }
            KeyEvent { code: KeyCode::Char('X'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.start_selection(/* negated = */ false);
            }
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.dialog =
                    Some(Dialog::rename(&self.active_tab().name));
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
                self.dialog =
                    Some(Dialog::search(SearchDirection::Forward));
            }
            KeyEvent { code: KeyCode::Char('?'), modifiers, .. }
                if modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT =>
            {
                self.dialog =
                    Some(Dialog::search(SearchDirection::Backward));
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
                let cloned = self.active_tab().filter.clone();
                self.push_tab(cloned);
                self.dialog =
                    Some(Dialog::filter(&self.active_tab().filter));
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
            KeyEvent {
                code: KeyCode::BackTab, ..
            }
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
            KeyEvent {
                code: KeyCode::Backspace, ..
            } => {
                self.backspace();
                EditAction::Handled
            }
            KeyEvent {
                code: KeyCode::Delete, ..
            } => {
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

    fn editor(&self) -> &LineEditor {
        match self {
            Self::Filter { editor, .. }
            | Self::Rename { editor }
            | Self::Search { editor, .. } => editor,
        }
    }

    fn parse_error(&self) -> Option<&str> {
        match self {
            Self::Filter { parse_error, .. }
            | Self::Search { parse_error, .. } => parse_error.as_deref(),
            Self::Rename { .. } => None,
        }
    }

    fn title(&self) -> &'static str {
        match self {
            Self::Filter { .. } => "Filter (Esc cancel · Enter apply)",
            Self::Rename { .. } => {
                "Rename tab (Esc cancel · Enter apply)"
            }
            Self::Search { .. } => "Search (Esc cancel · Enter apply)",
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
            | Self::Search { editor, .. } => editor.handle_edit(key),
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
        }
    }
}

/// Compact a [`regex::Error`]'s display into a single line, since the
/// search prompt has at most one line of room for it.  All whitespace
/// runs (including the embedded newlines `regex` uses to point at the
/// offending character) collapse to single spaces.
fn regex_error_summary(e: &regex::Error) -> String {
    e.to_string().split_whitespace().collect::<Vec<_>>().join(" ")
}

fn prev_char_boundary(s: &str, byte_idx: usize) -> usize {
    s[..byte_idx]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
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
    // Re-clamp in case the viewport just shrank past the previous top.
    let max_top = app.active_tab().max_top(app.viewport_height);
    if app.active_tab().viewport_top > max_top {
        app.active_tab_mut().viewport_top = max_top;
    }

    let tab = app.active_tab();
    let total = tab.formatted.len();
    let top = tab.viewport_top;
    let bottom = (top + content_area.height as usize).min(total);

    let lines: Vec<Line<'_>> = tab.formatted[top..bottom]
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let row_index = top + i;
            let mut line = match &tab.search {
                Some(search) => highlight_line(s, &search.regex),
                None => Line::raw(s.as_str()),
            };
            if tab.select.map(|s| s.row) == Some(row_index) {
                // Distinct from the search highlight (REVERSED on
                // matched runs); a row-wide background reads as "this
                // is the line you're about to act on" without fighting
                // search styling on the same row.
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
            let footer = if let Some(sel) = tab.select {
                let verb = if sel.negated { "exclude" } else { "include" };
                format!(
                    "{verb}: j/k select · Enter {verb} msg · \
                     Esc cancel · row {}/{}",
                    sel.row + 1,
                    total,
                )
            } else if total == 0 {
                format!(
                    "q quit · f filter · / search · </> step={} · \
                     x/X exclude/include · ^T new · ^W close · \
                     r rename · 0/0",
                    app.current_step_label(),
                )
            } else {
                format!(
                    "q quit · f filter · / search · </> step={} · \
                     x/X exclude/include · ^T new · ^W close · \
                     r rename · {}-{} of {}",
                    app.current_step_label(),
                    top + 1,
                    bottom,
                    total,
                )
            };
            frame.render_widget(Paragraph::new(footer), bottom_area);
        }
    }

    // Centered popups (Filter, Rename) draw on top of the rest.  The
    // Search prompt is laid out inline above and is skipped here.
    if let Some(dialog @ (Dialog::Filter { .. } | Dialog::Rename { .. })) =
        app.dialog.as_ref()
    {
        render_dialog(frame, dialog, area);
    }
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
    frame.render_widget(
        Paragraph::new(Line::raw(prompt_text)),
        prompt_area,
    );

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
            Paragraph::new(Line::raw(err))
                .style(Style::new().fg(Color::Red)),
            err_area,
        );
    }
}

fn render_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line<'_>> =
        app.tabs.iter().map(|t| Line::raw(t.name.as_str())).collect();
    let widget = Tabs::new(titles).select(app.active).highlight_style(
        Style::default().add_modifier(Modifier::REVERSED),
    );
    frame.render_widget(widget, area);
}

/// Carves a centered popup over `area` and draws either dialog variant.
///
/// The Filter variant additionally renders any parse error in red below
/// the edit row; the Rename variant simply leaves that row blank.
fn render_dialog(frame: &mut Frame, dialog: &Dialog, area: Rect) {
    let popup = popup_area(area, 70, 5);
    // Clear the underlying rows so the editor isn't drawn on top of
    // them.
    frame.render_widget(Clear, popup);

    let block = Block::bordered().title(dialog.title());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [edit_area, error_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    let editor = dialog.editor();
    frame.render_widget(
        Paragraph::new(Line::raw(editor.text.as_str())),
        edit_area,
    );

    // Cursor column: the dialog buffers are ASCII in practice (filter
    // syntax is ASCII, tab names typically too), so the byte offset
    // doubles as the column.  If we ever accept multibyte chars we'd
    // need to compute the display width here instead.
    let col = edit_area
        .x
        .saturating_add(u16::try_from(editor.cursor).unwrap_or(u16::MAX));
    let col = col.min(edit_area.x.saturating_add(edit_area.width));
    frame.set_cursor_position(Position::new(col, edit_area.y));

    if let Some(err) = dialog.parse_error()
        && error_area.height > 0
    {
        frame.render_widget(
            Paragraph::new(Line::raw(err))
                .style(Style::new().fg(Color::Red)),
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
    fn q_quits() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Char('q')));
        assert!(a.quit);
    }

    #[test]
    fn esc_quits() {
        let mut a = app(10, 5);
        a.handle_key(key(KeyCode::Esc));
        assert!(a.quit);
    }

    #[test]
    fn ctrl_c_quits() {
        let mut a = app(10, 5);
        a.handle_key(ctrl('c'));
        assert!(a.quit);
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
    }

    // ---------- top-level rendering ----------

    #[test]
    fn render_paints_rows_and_footer() {
        // Wider than 80 cols so the footer's trailing dynamic info
        // (step indicator, "1-3 of 3" counter) isn't truncated; the
        // live footer is sized to be legible at 80 cols even with
        // truncation.
        let backend = TestBackend::new(120, 6);
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
        assert_eq!(a.dialog.as_ref().unwrap().editor().text, "qj");
    }

    #[test]
    fn dialog_prepopulates_with_current_filter() {
        let f: Filter = "level>=warn name=Nexus".parse().unwrap();
        let d = Dialog::filter(&f);
        assert_eq!(d.editor().text, "level>=warn name=Nexus");
        // Cursor is at the end so the user can extend the filter
        // without homing first.
        assert_eq!(d.editor().cursor, d.editor().text.len());
        assert!(d.parse_error().is_none());
    }

    #[test]
    fn dialog_typing_inserts_at_cursor() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "name=Nexus");
        assert_eq!(d.editor().text, "name=Nexus");
        assert_eq!(d.editor().cursor, "name=Nexus".len());
    }

    #[test]
    fn dialog_backspace_deletes_char_before_cursor() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Backspace));
        assert_eq!(d.editor().text, "ab");
        assert_eq!(d.editor().cursor, 2);
    }

    #[test]
    fn dialog_left_right_move_cursor() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Left));
        assert_eq!(d.editor().cursor, 2);
        d.handle_key(key(KeyCode::Home));
        assert_eq!(d.editor().cursor, 0);
        d.handle_key(key(KeyCode::Right));
        assert_eq!(d.editor().cursor, 1);
        d.handle_key(key(KeyCode::End));
        assert_eq!(d.editor().cursor, 3);
    }

    #[test]
    fn dialog_delete_removes_char_after_cursor() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(key(KeyCode::Delete));
        assert_eq!(d.editor().text, "bc");
        assert_eq!(d.editor().cursor, 0);
    }

    #[test]
    fn dialog_ctrl_u_kills_to_start_of_line() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        // Position cursor inside "Nexus".
        for _ in 0..3 {
            d.handle_key(key(KeyCode::Left));
        }
        let cursor_before = d.editor().cursor;
        d.handle_key(ctrl('u'));
        assert_eq!(d.editor().text, "xus");
        assert_eq!(d.editor().cursor, 0);
        assert!(cursor_before > 0);
    }

    #[test]
    fn dialog_ctrl_u_at_start_is_noop() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "abc");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(ctrl('u'));
        assert_eq!(d.editor().text, "abc");
        assert_eq!(d.editor().cursor, 0);
    }

    #[test]
    fn dialog_ctrl_w_kills_previous_whitespace_word() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        d.handle_key(ctrl('w'));
        // The whole `name=Nexus` token disappears, plus the space.
        assert_eq!(d.editor().text, "level>=warn ");
        assert_eq!(d.editor().cursor, "level>=warn ".len());
    }

    #[test]
    fn dialog_ctrl_w_consumes_trailing_whitespace_first() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "name=Nexus   ");
        d.handle_key(ctrl('w'));
        assert_eq!(d.editor().text, "");
        assert_eq!(d.editor().cursor, 0);
    }

    #[test]
    fn dialog_alt_b_moves_back_one_alphanumeric_word() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        d.handle_key(alt('b'));
        assert_eq!(&d.editor().text[d.editor().cursor..], "Nexus");
        d.handle_key(alt('b'));
        assert_eq!(&d.editor().text[d.editor().cursor..], "name=Nexus");
        d.handle_key(alt('b'));
        assert_eq!(
            &d.editor().text[d.editor().cursor..],
            "warn name=Nexus",
        );
        d.handle_key(alt('b'));
        assert_eq!(d.editor().cursor, 0);
        // Once more: clamped at zero.
        d.handle_key(alt('b'));
        assert_eq!(d.editor().cursor, 0);
    }

    #[test]
    fn dialog_alt_f_moves_forward_one_alphanumeric_word() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "level>=warn name=Nexus");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(alt('f'));
        assert_eq!(&d.editor().text[..d.editor().cursor], "level");
        d.handle_key(alt('f'));
        assert_eq!(&d.editor().text[..d.editor().cursor], "level>=warn");
        d.handle_key(alt('f'));
        assert_eq!(
            &d.editor().text[..d.editor().cursor],
            "level>=warn name",
        );
        d.handle_key(alt('f'));
        assert_eq!(d.editor().cursor, d.editor().text.len());
        // Once more: clamped.
        d.handle_key(alt('f'));
        assert_eq!(d.editor().cursor, d.editor().text.len());
    }

    #[test]
    fn dialog_shows_parse_error_live() {
        let mut d = Dialog::filter(&Filter::default());
        type_into(&mut d, "bogus");
        assert!(d.parse_error().is_some());
        let len = d.editor().text.len();
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
        assert_eq!(a.active_tab().filter.to_string(), "level>=warn");
    }

    #[test]
    fn dialog_escape_discards_changes() {
        let mut a = app(10, 5);
        let original_filter = a.active_tab().filter.to_string();
        a.handle_key(key(KeyCode::Char('f')));
        type_into(a.dialog.as_mut().unwrap(), "name=Nexus");
        a.handle_key(key(KeyCode::Esc));
        assert!(a.dialog.is_none());
        assert_eq!(a.active_tab().filter.to_string(), original_filter);
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
            let drain = slog_bunyan::with_name("Nexus", file)
                .build()
                .fuse();
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
        assert_eq!(a.active_tab().filter.to_string(), "level>=warn");
        // Filter dialog is open with the cloned filter prefilled.
        let d = a.dialog.as_ref().expect("dialog should be open");
        assert!(matches!(d, Dialog::Filter { .. }));
        assert_eq!(d.editor().text, "level>=warn");
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
        assert_eq!(a.active_tab().filter.to_string(), "level>=warn");
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
        assert!(a.active_tab().filter.predicates().is_empty());
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
        assert_eq!(a.dialog.as_ref().unwrap().editor().text, "level>=warn ");
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
        assert_eq!(d.editor().text, "Tab 1");
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
        assert_eq!(d.editor().text, "");
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
        assert_eq!(a.dialog.as_ref().unwrap().editor().text, "alpha");
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
        assert_eq!(a.dialog.as_ref().unwrap().editor().text, "qj");
    }

    #[test]
    fn ctrl_w_inside_search_dialog_kills_word_not_close_tab() {
        let mut a = search_app();
        a.handle_key(key(KeyCode::Char('/')));
        type_into(a.dialog.as_mut().unwrap(), "alpha beta");
        a.handle_key(ctrl('w'));
        assert!(a.dialog.is_some());
        assert_eq!(a.dialog.as_ref().unwrap().editor().text, "alpha ");
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
        let rows: Vec<String> =
            ["foo", "bar", "foo bar", "baz", "qux foo"]
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

    fn excl_sel(row: usize) -> Selection {
        Selection { row, negated: true }
    }

    fn incl_sel(row: usize) -> Selection {
        Selection { row, negated: false }
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
        let before = a.active_tab().filter.to_string();
        a.handle_key(key(KeyCode::Char('x')));
        a.handle_key(key(KeyCode::Char('j')));
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.active_tab().select, None);
        assert_eq!(a.active_tab().filter.to_string(), before);
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
        let displayed = a.active_tab().filter.to_string();
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
        let displayed = a.active_tab().filter.to_string();
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
        let before = a.active_tab().filter.to_string();
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.active_tab().select, Some(excl_sel(1)));
        assert_eq!(a.active_tab().filter.to_string(), before);
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
        assert_eq!(a.dialog.as_ref().unwrap().editor().text, "=-><");
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
}
