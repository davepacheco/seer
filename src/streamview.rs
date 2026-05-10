// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Lazy windowed materialization for stream tabs.
//!
//! [`StreamView`] caches a bounded window of merged records around the
//! viewport's top.  As the user scrolls, the window slides: forward
//! extension fetches via [`Stepper::step_forward`], backward extension
//! via [`Stepper::step_backward`].  When the window grows past
//! [`WINDOW_SOFT_CAP`] records, entries are trimmed from the side
//! opposite the recent extension and the engine cursor for that side
//! is rewound by the trimmed records' bytes.
//!
//! `StreamView` owns plain data — no borrowed [`Engine`] reference —
//! and constructs a fresh [`Stepper`] on every fetch.  The `&Engine`
//! parameter on every fetching method threads the engine in by
//! reference.  Each refetch reopens the underlying file once per
//! [`BATCH_SIZE`]-sized batch, which is small relative to the parse
//! cost.
//!
//! Summary tabs do not use `StreamView`; they keep the existing
//! full-pass model since their output is bounded by the histogram
//! shape, not by the file size.

use crate::engine::{Cursor, Engine, MergeRecord};
use crate::event::Event;
use crate::filter::Filter;
use crate::render::{RenderOpts, format_event};
use crate::source::{ByteOffset, SourceId};
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use std::collections::VecDeque;
use std::time::{Duration as StdDuration, Instant};

/// Per-fetch batch size.  Each call to extend the window in either
/// direction asks the stepper for up to this many records.  Matches
/// the storage layer's batch size so we don't over-fetch.
const BATCH_SIZE: usize = 64;

/// Soft cap on cached records.  When extending in a direction would
/// push the window past this, we trim from the opposite end.
const WINDOW_SOFT_CAP: usize = 1024;

/// Minimum lines kept on either side of the viewport before triggering
/// extension.  When the viewport scrolls within `OVER_FETCH_LINES` of
/// an edge, the window grows in that direction so the user doesn't see
/// a blank tail.
const OVER_FETCH_LINES: usize = 128;

/// Cap on records walked per [`StreamView::search_step`] call before
/// returning [`SearchOutcome::BudgetExhausted`].  A regex that matches
/// nothing on a 100M-line file would otherwise scan the whole thing on
/// every `n`.  Exposed so the TUI can name the bound in user-facing
/// notices.
pub const SEARCH_BUDGET: usize = 50_000;

/// Stable identity for a record in the merged stream.
///
/// `(source_id, offset)` uniquely identifies a record across window
/// slides and trims, so it's safe to use as the user's selection or
/// viewport anchor even when the underlying [`StreamView`]'s record
/// indices shift.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RecordKey {
    pub source_id: SourceId,
    pub offset: ByteOffset,
}

impl RecordKey {
    /// Builds a [`RecordKey`] from a [`MergeRecord`].
    pub fn from_record(record: &MergeRecord) -> Self {
        Self {
            source_id: record.source_id.clone(),
            offset: record.offset,
        }
    }
}

/// Per-record entry held in the window: the merged record itself plus
/// its pre-formatted display lines under the active [`RenderOpts`].
/// `lines.len() >= 1` always: a parse error contributes a single error
/// line; a real event contributes a header plus zero or more
/// extra-field lines.
struct WindowEntry {
    record: MergeRecord,
    lines: Vec<String>,
}

impl WindowEntry {
    fn new(record: MergeRecord, opts: &RenderOpts) -> Self {
        let lines = format_record(&record, opts);
        Self { record, lines }
    }

    fn key(&self) -> RecordKey {
        RecordKey::from_record(&self.record)
    }
}

/// Renders a [`MergeRecord`] into one or more display lines.
///
/// `Ok` events use [`format_event`] (header plus extras); `Err` events
/// produce a single line carrying the [`MergeError`]'s `Display`
/// message — matching the existing TUI behavior of surfacing parse
/// errors inline next to events.
fn format_record(record: &MergeRecord, opts: &RenderOpts) -> Vec<String> {
    match &record.event {
        Ok(event) => format_event(event, opts),
        Err(err) => vec![err.to_string()],
    }
}

/// Where the viewport's top line sits.
///
/// Anchored by `(record_key, line_within_record)` rather than a flat
/// line index so the position survives window trims and slides — the
/// flat index would shift whenever the front of the deque moved.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Anchor {
    /// Top of viewport sits at line `line` within the record identified
    /// by `key`.  `line == 0` is the record's header line; positive
    /// values index into its extras (only when `show_extras` is on).
    On { key: RecordKey, line: usize },
    /// Window is empty under the active filter.
    Empty,
    /// Anchor logically at "before the first record" — the next
    /// extension will resolve to the front-most record.
    PinFront,
    /// Anchor logically at "after the last record" — the next
    /// extension will resolve to the back-most record.
    PinBack,
}

/// Running parse statistics over the StreamView's lifetime since the
/// last filter change.  Each fetch (forward or backward extension)
/// adds the records it pulled, the bytes those records sum to, and the
/// wall-clock time spent.
#[derive(Clone, Debug, Default)]
pub struct ParseStats {
    pub records: u64,
    pub bytes: u64,
    pub elapsed: StdDuration,
}

/// Outcome of a [`StreamView::search_step`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchOutcome {
    /// A match was found; the viewport now sits on it.
    Found,
    /// The search ran to the end of the stream (in the chosen
    /// direction) without finding a match.
    NotFound,
    /// The search hit its per-call budget.  The viewport position is
    /// unchanged.  Calling `search_step` again with the same regex and
    /// direction, *without* navigating in between, resumes from where
    /// the previous call stopped — see [`StreamView::search_step`] for
    /// the conditions that invalidate the resume point.
    BudgetExhausted,
    /// The caller's `cancel` callback returned `true` mid-scan.  The
    /// viewport position is unchanged and no resume point is saved, so
    /// a follow-up `search_step` starts fresh from the current anchor
    /// rather than picking up where this one stopped.
    Cancelled,
}

/// Direction of a search step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchDir {
    Forward,
    Backward,
}

/// Direction of `<` / `>` time navigation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimeDir {
    Forward,
    Backward,
}

/// Snapshot of where a budget-exhausted search stopped, so a follow-up
/// call can resume scanning instead of restarting from the anchor.
///
/// The resume is honored only when (a) the regex pattern matches, (b)
/// the direction matches, and (c) the anchor hasn't moved since the
/// snapshot — the first two guard against switching to a different
/// search, the third detects user navigation between calls.  When the
/// snapshot's record has been trimmed out of the window the snapshot
/// is silently dropped (the user has scrolled far away; resuming would
/// re-fetch from byte 0 anyway).
#[derive(Clone, Debug)]
struct SearchResumePoint {
    pattern: String,
    direction: SearchDir,
    anchor_when_set: Anchor,
    record_key: RecordKey,
    /// Forward: lowest line within the record to scan next.
    /// Backward: highest line within the record to scan next.
    line_bound: usize,
}

/// Lazy-windowed materialization for one stream tab's view of the
/// merged event stream.
///
/// Owns the cached window of records and their formatted display
/// lines; threads `&Engine` in on every fetch.  Tracks the viewport's
/// top by `(record_key, line_within_record)` rather than a flat line
/// index so it stays correct across trims.
pub struct StreamView {
    filter: Filter,
    opts: RenderOpts,
    /// Cursor at the front of the window: a stepper built at this
    /// cursor's `step_forward` would emit `records.front()` first; its
    /// `step_backward` would emit the record just before the window's
    /// front.
    front_cursor: Cursor,
    /// Cursor at the back of the window: a stepper built at this
    /// cursor's `step_forward` would emit the record just after the
    /// window's back; its `step_backward` would emit `records.back()`.
    back_cursor: Cursor,
    records: VecDeque<WindowEntry>,
    forward_eof: bool,
    backward_eof: bool,
    anchor: Anchor,
    parse_stats: ParseStats,
    /// Set when a search returns [`SearchOutcome::BudgetExhausted`] so
    /// the next `search_step` call can pick up where this one stopped.
    /// Cleared on `Found` and `NotFound`; ignored when the regex,
    /// direction, or anchor changes between calls.
    search_resume: Option<SearchResumePoint>,
}

impl StreamView {
    /// Constructs an empty view at the start of the engine's content.
    /// The window is populated lazily on the first call to a method
    /// that requires content (e.g. [`Self::ensure_window`]).
    pub fn new(filter: Filter, opts: RenderOpts) -> Self {
        Self {
            filter,
            opts,
            front_cursor: Cursor::new(),
            back_cursor: Cursor::new(),
            records: VecDeque::new(),
            forward_eof: false,
            backward_eof: false,
            anchor: Anchor::PinFront,
            parse_stats: ParseStats::default(),
            search_resume: None,
        }
    }

    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// Returns the active rendering options as a value, by-copy.
    pub fn render_opts(&self) -> RenderOpts {
        self.opts
    }

    pub fn parse_stats(&self) -> &ParseStats {
        &self.parse_stats
    }

    /// Iterates over the cached records and their pre-formatted display
    /// lines, in time order (front of deque first).  Used by callers
    /// that need to walk the window directly (e.g. to materialize a
    /// flat rendering buffer).
    pub fn records(&self) -> impl Iterator<Item = (&MergeRecord, &[String])> {
        self.records.iter().map(|e| (&e.record, e.lines.as_slice()))
    }

    /// Returns the flat line index (across the window's records) at
    /// which the viewport's anchor sits, or `0` when the window is
    /// empty.  Caller's flat index has the same meaning as the
    /// existing TUI's `viewport_top`.
    pub fn anchor_flat_line(&self) -> usize {
        if self.records.is_empty() {
            return 0;
        }
        let (idx, line) = self.anchor_indices();
        let prefix: usize =
            self.records.iter().take(idx).map(|e| e.lines.len()).sum();
        prefix + line
    }

    /// Sets the anchor to the record/line at flat line index
    /// `flat_line` within the current window.  Out-of-range values
    /// clamp to the nearest valid line.
    pub fn set_anchor_to_flat_line(&mut self, flat_line: usize) {
        if self.records.is_empty() {
            self.anchor = Anchor::Empty;
            return;
        }
        let mut remaining = flat_line;
        for entry in self.records.iter() {
            if remaining < entry.lines.len() {
                self.anchor = Anchor::On {
                    key: entry.key(),
                    line: remaining,
                };
                return;
            }
            remaining -= entry.lines.len();
        }
        // Past end: clamp to last record's last line.
        let last = self.records.back().unwrap();
        self.anchor =
            Anchor::On { key: last.key(), line: last.lines.len() - 1 };
    }

    /// Returns the record at the viewport's top, if any.
    pub fn anchor_record(&self) -> Option<&MergeRecord> {
        match &self.anchor {
            Anchor::On { key, .. } => self.find_record(key),
            _ => None,
        }
    }

    /// Returns the viewport's anchor as `(record_key, line_within)`,
    /// or `None` when the window is empty.  The TUI's footer uses this
    /// to render a "you are here" indicator.
    pub fn anchor_position(&self) -> Option<(RecordKey, usize)> {
        match &self.anchor {
            Anchor::On { key, line } => Some((key.clone(), *line)),
            _ => None,
        }
    }

    /// Returns a [`Cursor`] that, when fed back into
    /// [`Self::seek_to_cursor`], lands the viewport on the same record
    /// the anchor is currently on.  Used by the TUI on filter changes:
    /// it captures the anchor before refresh so the new view can
    /// resume on (or near) the same record under the new filter,
    /// instead of snapping back to the top.
    ///
    /// Returns `None` when the window is empty or the anchor isn't
    /// pinned to a specific record.
    pub fn cursor_at_anchor(&self) -> Option<Cursor> {
        match &self.anchor {
            Anchor::On { key, .. } => {
                let idx = self.find_record_idx(key)?;
                self.cursor_before_record(idx)
            }
            _ => None,
        }
    }

    fn find_record(&self, key: &RecordKey) -> Option<&MergeRecord> {
        self.records
            .iter()
            .find(|e| {
                e.record.source_id == key.source_id
                    && e.record.offset == key.offset
            })
            .map(|e| &e.record)
    }

    fn find_record_idx(&self, key: &RecordKey) -> Option<usize> {
        self.records.iter().position(|e| {
            e.record.source_id == key.source_id
                && e.record.offset == key.offset
        })
    }

    /// Looks up the record at `key` and returns it, if currently
    /// cached in the window.
    pub fn record_by_key(&self, key: &RecordKey) -> Option<&MergeRecord> {
        self.find_record(key)
    }

    /// Returns the [`Cursor`] "just before" the record at window index
    /// `idx` — i.e., a cursor such that
    /// `engine.stepper(filter, &cursor)`'s next `step_forward` (under
    /// the same filter the window was built with) returns that record.
    ///
    /// Derived from `front_cursor` plus the trailing records preceding
    /// `idx` in the window — no I/O, no merge walk.  Returns `None`
    /// when `idx` is out of range.
    pub fn cursor_before_record(&self, idx: usize) -> Option<Cursor> {
        if idx >= self.records.len() {
            return None;
        }
        let mut cursor = self.front_cursor.clone();
        for entry in self.records.iter().take(idx) {
            let r = &entry.record;
            cursor.set(
                r.source_id.clone(),
                ByteOffset::from(r.offset.get() + r.length),
            );
        }
        Some(cursor)
    }

    /// Returns true iff the window is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Returns the number of cached records.
    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    /// Total display lines across all cached records.
    pub fn total_lines(&self) -> usize {
        self.records.iter().map(|e| e.lines.len()).sum()
    }

    /// Replaces the active filter, dropping cached records (since they
    /// were filtered against the old filter) and reseting both cursors
    /// to the start.  Equivalent to "filter changed, restart from the
    /// top."
    ///
    /// Cursors are reset to [`Cursor::new`] (= byte 0 for every source)
    /// rather than keeping the current viewport position because the
    /// existing TUI's filter-change semantics resets `viewport_top` to
    /// 0; matching that here keeps user expectations stable.
    pub fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
        self.records.clear();
        self.front_cursor = Cursor::new();
        self.back_cursor = Cursor::new();
        self.forward_eof = false;
        self.backward_eof = false;
        self.anchor = Anchor::PinFront;
        self.parse_stats = ParseStats::default();
    }

    /// Replaces the active rendering options and reformats every cached
    /// record so the display lines reflect the new settings.  When
    /// `show_extras` is the dimension that changed, the anchor's line
    /// offset is snapped to 0 of the same record, since the line count
    /// per record can collapse or expand and the user's exact
    /// line-within-record can't be preserved.  All other dimensions
    /// (date prefix, hostname mode, name/pid visibility) only change
    /// the header line's width, leaving the line count per record
    /// unchanged — so the anchor's line offset is preserved.
    pub fn set_render_opts(&mut self, opts: RenderOpts) {
        if opts == self.opts {
            return;
        }
        let extras_changed = opts.show_extras != self.opts.show_extras;
        self.opts = opts;
        self.reformat_window();
        if extras_changed
            && let Anchor::On { line, .. } = &mut self.anchor
        {
            *line = 0;
        }
    }

    /// Re-runs [`format_record`] for every cached entry against the
    /// view's current rendering knobs.  Shared by [`Self::set_render_opts`]
    /// so adding another knob doesn't need its own copy of the loop.
    fn reformat_window(&mut self) {
        let opts = self.opts;
        for entry in &mut self.records {
            entry.lines = format_record(&entry.record, &opts);
        }
    }

    /// Resets the view to the start of the merged stream and ensures
    /// the window covers `viewport_height + OVER_FETCH_LINES` lines.
    pub fn seek_to_start(
        &mut self,
        engine: &Engine,
        viewport_height: u16,
    ) {
        self.records.clear();
        self.front_cursor = Cursor::new();
        self.back_cursor = Cursor::new();
        self.forward_eof = false;
        self.backward_eof = false;
        self.anchor = Anchor::PinFront;
        self.ensure_window(engine, viewport_height);
    }

    /// Resets the view to the end of the merged stream and ensures
    /// the window covers `viewport_height + OVER_FETCH_LINES` lines.
    pub fn seek_to_end(
        &mut self,
        engine: &Engine,
        viewport_height: u16,
    ) -> std::io::Result<()> {
        let end = engine.cursor_at_end()?;
        self.records.clear();
        self.front_cursor = end.clone();
        self.back_cursor = end;
        self.forward_eof = true;
        self.backward_eof = false;
        self.anchor = Anchor::PinBack;
        self.ensure_window(engine, viewport_height);
        Ok(())
    }

    /// Rebuilds the view at `cursor`, fetching enough records to fill
    /// the viewport.  The viewport anchors on the first record
    /// returned by a forward step from `cursor` under the active
    /// filter; if no record at or after `cursor` survives the filter,
    /// falls back to the last record before `cursor` that does.  This
    /// matches the TUI's filter-change and bookmark-navigation
    /// semantics: a saved cursor keeps working as a marker even when
    /// the bookmarked event is hidden, by sliding to the nearest
    /// visible neighbor on either side rather than yielding an empty
    /// view.
    pub fn seek_to_cursor(
        &mut self,
        engine: &Engine,
        cursor: Cursor,
        viewport_height: u16,
    ) {
        self.records.clear();
        self.front_cursor = cursor.clone();
        self.back_cursor = cursor;
        self.forward_eof = false;
        self.backward_eof = false;
        self.anchor = Anchor::PinFront;
        self.ensure_window(engine, viewport_height);
        // No record at or after the cursor passes the filter — try
        // backward.  We swap to PinBack semantics here rather than
        // calling `seek_to_end`, which would walk the whole stream
        // backwards from EOF; here we want the *closest* visible
        // record before `cursor`, which is at most a batch away.
        if self.records.is_empty() && !self.backward_eof {
            self.anchor = Anchor::PinBack;
            self.ensure_window(engine, viewport_height);
        }
    }

    /// Ensures the window has enough records to render the viewport
    /// plus an over-fetch buffer in each direction.  Cheap when the
    /// window is already populated.
    pub fn ensure_window(&mut self, engine: &Engine, viewport_height: u16) {
        let target_lines =
            viewport_height as usize + OVER_FETCH_LINES;
        // Initial population: fetch forward (or backward when pinned
        // to the back) until we have either enough lines or hit EOF.
        if self.records.is_empty() {
            match self.anchor {
                Anchor::PinBack => {
                    self.extend_backward_until(
                        engine,
                        |records, _| total_lines(records) >= target_lines,
                    );
                    if let Some(entry) = self.records.back() {
                        self.anchor = Anchor::On {
                            key: entry.key(),
                            line: entry.lines.len().saturating_sub(1),
                        };
                    } else {
                        self.anchor = Anchor::Empty;
                    }
                }
                _ => {
                    self.extend_forward_until(
                        engine,
                        |records, _| total_lines(records) >= target_lines,
                    );
                    if let Some(entry) = self.records.front() {
                        self.anchor = Anchor::On {
                            key: entry.key(),
                            line: 0,
                        };
                    } else {
                        self.anchor = Anchor::Empty;
                    }
                }
            }
        }
    }

    /// Fetches up to `BATCH_SIZE` records forward and appends them.
    /// Returns the number actually fetched.
    fn extend_forward_batch(&mut self, engine: &Engine) -> usize {
        if self.forward_eof {
            return 0;
        }
        let started = Instant::now();
        let mut stepper =
            engine.stepper(self.filter.clone(), &self.back_cursor);
        let mut fetched = 0;
        let mut bytes = 0u64;
        for _ in 0..BATCH_SIZE {
            match stepper.step_forward() {
                Some(record) => {
                    bytes += record.length;
                    fetched += 1;
                    self.records
                        .push_back(WindowEntry::new(record, &self.opts));
                }
                None => {
                    self.forward_eof = true;
                    break;
                }
            }
        }
        self.back_cursor = stepper.cursor();
        self.parse_stats.records += fetched as u64;
        self.parse_stats.bytes += bytes;
        self.parse_stats.elapsed += started.elapsed();
        fetched
    }

    /// Fetches up to `BATCH_SIZE` records backward and prepends them.
    /// Returns the number actually fetched.
    fn extend_backward_batch(&mut self, engine: &Engine) -> usize {
        if self.backward_eof {
            return 0;
        }
        let started = Instant::now();
        let mut stepper =
            engine.stepper(self.filter.clone(), &self.front_cursor);
        let mut fetched = 0;
        let mut bytes = 0u64;
        // step_backward returns records in reverse time order; we
        // push them to the front, so the deque stays sorted oldest
        // first.
        for _ in 0..BATCH_SIZE {
            match stepper.step_backward() {
                Some(record) => {
                    bytes += record.length;
                    fetched += 1;
                    self.records
                        .push_front(WindowEntry::new(record, &self.opts));
                }
                None => {
                    self.backward_eof = true;
                    break;
                }
            }
        }
        self.front_cursor = stepper.cursor();
        self.parse_stats.records += fetched as u64;
        self.parse_stats.bytes += bytes;
        self.parse_stats.elapsed += started.elapsed();
        fetched
    }

    /// Repeatedly extends forward in `BATCH_SIZE` chunks until `done`
    /// returns true, EOF is reached, or we exceed `WINDOW_SOFT_CAP`.
    /// `done` is called with the current records and the new entries
    /// pushed in the last batch.
    fn extend_forward_until<F>(&mut self, engine: &Engine, mut done: F)
    where
        F: FnMut(&VecDeque<WindowEntry>, usize) -> bool,
    {
        loop {
            if done(&self.records, 0) {
                return;
            }
            let fetched = self.extend_forward_batch(engine);
            if fetched == 0 {
                return;
            }
            if done(&self.records, fetched) {
                return;
            }
            if self.records.len() > WINDOW_SOFT_CAP {
                return;
            }
        }
    }

    /// Symmetric to `extend_forward_until` for the backward direction.
    fn extend_backward_until<F>(&mut self, engine: &Engine, mut done: F)
    where
        F: FnMut(&VecDeque<WindowEntry>, usize) -> bool,
    {
        loop {
            if done(&self.records, 0) {
                return;
            }
            let fetched = self.extend_backward_batch(engine);
            if fetched == 0 {
                return;
            }
            if done(&self.records, fetched) {
                return;
            }
            if self.records.len() > WINDOW_SOFT_CAP {
                return;
            }
        }
    }

    /// Trims records from the front of the window down to
    /// `WINDOW_SOFT_CAP`, advancing `front_cursor` past the trimmed
    /// records so a backward fetch resumes correctly.  Never trims
    /// records that contain or follow the anchor.
    fn trim_front(&mut self) {
        while self.records.len() > WINDOW_SOFT_CAP {
            // Don't trim the anchor or anything past it.
            let anchor_idx = match &self.anchor {
                Anchor::On { key, .. } => self.find_record_idx(key),
                _ => None,
            };
            if anchor_idx.is_none_or(|i| i == 0) {
                break;
            }
            let entry = self.records.pop_front().unwrap();
            // Advance front_cursor past the trimmed record so the
            // next backward refetch sees it again.  For the source
            // we just trimmed from, the new front offset is
            // (offset + length).  Other sources unchanged.
            self.front_cursor.set(
                entry.record.source_id.clone(),
                ByteOffset::from(
                    entry.record.offset.get() + entry.record.length,
                ),
            );
            // Trimming exposes earlier territory for backward fetches.
            self.backward_eof = false;
        }
    }

    /// Symmetric to `trim_front`.
    fn trim_back(&mut self) {
        while self.records.len() > WINDOW_SOFT_CAP {
            let anchor_idx = match &self.anchor {
                Anchor::On { key, .. } => self.find_record_idx(key),
                _ => None,
            };
            let last = self.records.len() - 1;
            if anchor_idx.is_none_or(|i| i == last) {
                break;
            }
            let entry = self.records.pop_back().unwrap();
            self.back_cursor
                .set(entry.record.source_id.clone(), entry.record.offset);
            self.forward_eof = false;
        }
    }

    /// Scrolls the viewport by `delta` display lines (positive =
    /// forward).  Extends the window in the direction of motion when
    /// the new anchor would lie within `OVER_FETCH_LINES` of an edge.
    pub fn scroll_lines(
        &mut self,
        engine: &Engine,
        delta: isize,
        viewport_height: u16,
    ) {
        if self.records.is_empty() {
            self.ensure_window(engine, viewport_height);
            if self.records.is_empty() {
                return;
            }
        }
        if delta == 0 {
            return;
        }
        // Resolve the current anchor to a (record_idx, line_within)
        // pair into `records`; PinFront → (0, 0), PinBack → last.
        let (mut idx, mut line) = self.anchor_indices();
        let mut remaining = delta;
        while remaining != 0 {
            if remaining > 0 {
                // Forward: consume remaining lines in the current
                // record, then advance to the next.
                let lines_in_record = self.records[idx].lines.len();
                let lines_left = lines_in_record - 1 - line;
                if (remaining as usize) <= lines_left {
                    line += remaining as usize;
                    break;
                }
                remaining -= (lines_left + 1) as isize;
                if idx + 1 < self.records.len() {
                    idx += 1;
                    line = 0;
                } else if !self.forward_eof {
                    self.extend_forward_batch(engine);
                    self.trim_front();
                    let (new_idx, _) = self.anchor_indices();
                    idx = new_idx;
                    if idx + 1 < self.records.len() {
                        idx += 1;
                        line = 0;
                    } else {
                        // Forward EOF on the just-fetched batch; stay
                        // at the last line of the last record.
                        line = self.records[idx].lines.len() - 1;
                        break;
                    }
                } else {
                    // EOF and no next record; clamp to last line.
                    line = self.records[idx].lines.len() - 1;
                    break;
                }
            } else {
                // Backward: consume remaining lines back to the start
                // of the current record, then to the previous record.
                let to_top = line as isize;
                if -remaining <= to_top {
                    line = (line as isize + remaining) as usize;
                    break;
                }
                remaining += (line + 1) as isize;
                if idx > 0 {
                    idx -= 1;
                    line = self.records[idx].lines.len() - 1;
                } else if !self.backward_eof {
                    self.extend_backward_batch(engine);
                    self.trim_back();
                    let (new_idx, _) = self.anchor_indices();
                    idx = new_idx;
                    if idx > 0 {
                        idx -= 1;
                        line = self.records[idx].lines.len() - 1;
                    } else {
                        line = 0;
                        break;
                    }
                } else {
                    line = 0;
                    break;
                }
            }
            // Update anchor each iteration so anchor_indices() is
            // consistent for the next refetch's index computation.
            self.anchor = Anchor::On {
                key: self.records[idx].key(),
                line,
            };
        }
        self.anchor =
            Anchor::On { key: self.records[idx].key(), line };
    }

    /// Resolves the current anchor to `(record_idx, line)` in the
    /// `records` deque.  Caller must ensure the window is non-empty.
    fn anchor_indices(&self) -> (usize, usize) {
        match &self.anchor {
            Anchor::On { key, line } => match self.find_record_idx(key) {
                Some(idx) => (idx, *line),
                None => (0, 0),
            },
            Anchor::Empty | Anchor::PinFront => (0, 0),
            Anchor::PinBack => {
                let last = self.records.len() - 1;
                (last, self.records[last].lines.len() - 1)
            }
        }
    }

    /// Advances the viewport by `delta` of wall-clock time, using the
    /// anchor record's timestamp as the starting point.  For positive
    /// deltas the new anchor is the first event with `time >=
    /// anchor.time + delta`; for negative deltas, the latest event
    /// with `time <= anchor.time + delta`.
    ///
    /// Walks via stepper, fetching only enough records to reach the
    /// target.  No-op when the anchor record has no timestamp (parse
    /// error or empty window).
    pub fn advance_time(
        &mut self,
        engine: &Engine,
        delta: Duration,
        viewport_height: u16,
    ) {
        let dir = if delta.num_milliseconds() >= 0 {
            TimeDir::Forward
        } else {
            TimeDir::Backward
        };
        let Some(anchor_time) = self.anchor_event_time(dir) else {
            return;
        };
        let target = anchor_time + delta;
        match dir {
            TimeDir::Forward => {
                self.advance_time_forward(engine, target, viewport_height)
            }
            TimeDir::Backward => {
                self.advance_time_backward(engine, target, viewport_height)
            }
        }
    }

    /// Returns the timestamp of the closest event to the anchor in
    /// the requested direction (preferring same-direction; falling
    /// back to opposite).  None when no event is in the window.
    fn anchor_event_time(&self, dir: TimeDir) -> Option<DateTime<Utc>> {
        if self.records.is_empty() {
            return None;
        }
        let (anchor_idx, _) = self.anchor_indices();
        let event_time = |i: usize| -> Option<DateTime<Utc>> {
            self.records[i]
                .record
                .event
                .as_ref()
                .ok()
                .map(|e: &Event| e.time)
        };
        match dir {
            TimeDir::Forward => (anchor_idx..self.records.len())
                .find_map(event_time)
                .or_else(|| (0..anchor_idx).rev().find_map(event_time)),
            TimeDir::Backward => (0..=anchor_idx)
                .rev()
                .find_map(event_time)
                .or_else(|| {
                    ((anchor_idx + 1)..self.records.len())
                        .find_map(event_time)
                }),
        }
    }

    fn advance_time_forward(
        &mut self,
        engine: &Engine,
        target: DateTime<Utc>,
        viewport_height: u16,
    ) {
        let (mut idx, _) = self.anchor_indices();
        // Walk cached records forward until we find one with time >=
        // target.
        loop {
            while idx < self.records.len() {
                if let Ok(ev) = &self.records[idx].record.event
                    && ev.time >= target
                {
                    let key = self.records[idx].key();
                    self.anchor = Anchor::On { key, line: 0 };
                    self.ensure_window(engine, viewport_height);
                    return;
                }
                idx += 1;
            }
            if self.forward_eof {
                // Snap to the last record.
                let last = self.records.len() - 1;
                let key = self.records[last].key();
                self.anchor = Anchor::On {
                    key,
                    line: self.records[last]
                        .lines
                        .len()
                        .saturating_sub(1),
                };
                return;
            }
            let fetched = self.extend_forward_batch(engine);
            if fetched == 0 {
                continue;
            }
            // Trim the front if we've grown too large; carefully not
            // past the anchor or the target.  Safe to trim because the
            // anchor is somewhere before idx and we'll re-anchor after
            // finding the target.
            self.trim_front();
            let (new_idx, _) = self.anchor_indices();
            idx = new_idx;
            // After potential trim, idx may need to advance again.
        }
    }

    fn advance_time_backward(
        &mut self,
        engine: &Engine,
        target: DateTime<Utc>,
        viewport_height: u16,
    ) {
        loop {
            let (anchor_idx, _) = self.anchor_indices();
            // Walk cached records backward until we find one with
            // time <= target.
            for i in (0..=anchor_idx).rev() {
                if let Ok(ev) = &self.records[i].record.event
                    && ev.time <= target
                {
                    let key = self.records[i].key();
                    self.anchor = Anchor::On { key, line: 0 };
                    self.ensure_window(engine, viewport_height);
                    return;
                }
            }
            if self.backward_eof {
                let key = self.records.front().unwrap().key();
                self.anchor = Anchor::On { key, line: 0 };
                return;
            }
            let fetched = self.extend_backward_batch(engine);
            if fetched == 0 {
                continue;
            }
            self.trim_back();
        }
    }

    /// Searches for the next line matching `regex` in `direction`,
    /// stepping through the merged stream as needed.  When found, the
    /// viewport anchor moves to the matching line.
    ///
    /// `exclusive`: if true, skips the current anchor's line in the
    /// initial scan (so `n` after a previous match doesn't re-find the
    /// same line); if false, includes it (the initial `/<pattern>` does
    /// match a line at the cursor's current position if applicable).
    ///
    /// Walks at most [`SEARCH_BUDGET`] records per call before
    /// returning [`SearchOutcome::BudgetExhausted`].  When that
    /// happens, the next `search_step` call with the same regex,
    /// direction, and an unchanged anchor resumes from where this one
    /// stopped; switching regex or direction or moving the anchor
    /// (e.g. by scrolling) drops the resume point and restarts from
    /// the anchor.
    ///
    /// `cancel` is consulted once per scanned record; returning `true`
    /// aborts the scan with [`SearchOutcome::Cancelled`], leaving the
    /// anchor unchanged and saving no resume point.  Callers that have
    /// no cancellation source can pass `&mut || false`.
    pub fn search_step(
        &mut self,
        engine: &Engine,
        regex: &Regex,
        direction: SearchDir,
        exclusive: bool,
        viewport_height: u16,
        cancel: &mut dyn FnMut() -> bool,
    ) -> SearchOutcome {
        self.search_step_with_budget(
            engine,
            regex,
            direction,
            exclusive,
            viewport_height,
            SEARCH_BUDGET,
            cancel,
        )
    }

    /// Same as [`Self::search_step`] but with a caller-supplied
    /// budget.  Available to `pub(crate)` so streamview's own tests can
    /// drive [`SearchOutcome::BudgetExhausted`] without having to build
    /// 50,000-record fixtures.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_step_with_budget(
        &mut self,
        engine: &Engine,
        regex: &Regex,
        direction: SearchDir,
        exclusive: bool,
        viewport_height: u16,
        budget: usize,
        cancel: &mut dyn FnMut() -> bool,
    ) -> SearchOutcome {
        if self.records.is_empty() {
            self.ensure_window(engine, viewport_height);
            if self.records.is_empty() {
                return SearchOutcome::NotFound;
            }
        }
        let resume_idx = self.consume_valid_resume(regex, direction);
        let mut budget = budget;
        match direction {
            SearchDir::Forward => self.search_step_forward(
                engine,
                regex,
                exclusive,
                &mut budget,
                viewport_height,
                resume_idx,
                cancel,
            ),
            SearchDir::Backward => self.search_step_backward(
                engine,
                regex,
                exclusive,
                &mut budget,
                viewport_height,
                resume_idx,
                cancel,
            ),
        }
    }

    /// Takes any saved resume point, validates it against the requested
    /// scan, and returns the resolved `(record_idx, line_bound)` or
    /// `None` if it doesn't apply.  A stale resume is dropped on the
    /// way through so it can't outlive its window.
    fn consume_valid_resume(
        &mut self,
        regex: &Regex,
        direction: SearchDir,
    ) -> Option<(usize, usize)> {
        let resume = self.search_resume.take()?;
        if resume.pattern != regex.as_str()
            || resume.direction != direction
            || resume.anchor_when_set != self.anchor
        {
            return None;
        }
        let idx = self.find_record_idx(&resume.record_key)?;
        Some((idx, resume.line_bound))
    }

    /// Stores a resume point capturing where this scan stopped, so a
    /// follow-up `search_step` with the same regex/direction and an
    /// unchanged anchor can pick up where it left off.
    fn save_search_resume(
        &mut self,
        regex: &Regex,
        direction: SearchDir,
        record_idx: usize,
        line_bound: usize,
    ) {
        self.search_resume = Some(SearchResumePoint {
            pattern: regex.as_str().to_string(),
            direction,
            anchor_when_set: self.anchor.clone(),
            record_key: self.records[record_idx].key(),
            line_bound,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn search_step_forward(
        &mut self,
        engine: &Engine,
        regex: &Regex,
        exclusive: bool,
        budget: &mut usize,
        viewport_height: u16,
        resume: Option<(usize, usize)>,
        cancel: &mut dyn FnMut() -> bool,
    ) -> SearchOutcome {
        let (mut idx, mut start_line) = resume.unwrap_or_else(|| {
            let (anchor_idx, anchor_line) = self.anchor_indices();
            (anchor_idx, if exclusive { anchor_line + 1 } else { anchor_line })
        });
        loop {
            while idx < self.records.len() {
                if cancel() {
                    return SearchOutcome::Cancelled;
                }
                if *budget == 0 {
                    self.save_search_resume(
                        regex,
                        SearchDir::Forward,
                        idx,
                        start_line,
                    );
                    return SearchOutcome::BudgetExhausted;
                }
                *budget -= 1;
                let lines = &self.records[idx].lines;
                if let Some(hit) = (start_line..lines.len())
                    .find(|&i| regex.is_match(&lines[i]))
                {
                    let key = self.records[idx].key();
                    self.anchor = Anchor::On { key, line: hit };
                    self.ensure_window(engine, viewport_height);
                    return SearchOutcome::Found;
                }
                idx += 1;
                start_line = 0;
            }
            if self.forward_eof {
                return SearchOutcome::NotFound;
            }
            let fetched = self.extend_forward_batch(engine);
            if fetched == 0 {
                continue;
            }
            self.trim_front();
            // Newly fetched records are at the back of the deque,
            // i.e. indices [old_len - fetched .. records.len()).
            // After trim_front we can locate the first new record by
            // counting back from records.len().
            idx = self.records.len() - fetched;
            start_line = 0;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn search_step_backward(
        &mut self,
        engine: &Engine,
        regex: &Regex,
        exclusive: bool,
        budget: &mut usize,
        viewport_height: u16,
        resume: Option<(usize, usize)>,
        cancel: &mut dyn FnMut() -> bool,
    ) -> SearchOutcome {
        let (mut idx, mut end_line): (isize, isize) = match resume {
            Some((i, line)) => (i as isize, line as isize),
            None => {
                let (anchor_idx, anchor_line) = self.anchor_indices();
                let mut idx = anchor_idx as isize;
                // Initial scan upper bound: the anchor's line, or one
                // before it under `exclusive`.  When `exclusive` and
                // the anchor is on line 0, skip the current record
                // entirely and start at the previous one.
                let end = if exclusive && anchor_line == 0 {
                    idx -= 1;
                    if idx < 0 {
                        -1
                    } else {
                        self.records[idx as usize].lines.len() as isize - 1
                    }
                } else if exclusive {
                    anchor_line.saturating_sub(1) as isize
                } else {
                    anchor_line as isize
                };
                (idx, end)
            }
        };
        loop {
            while idx >= 0 {
                if cancel() {
                    return SearchOutcome::Cancelled;
                }
                if *budget == 0 {
                    // end_line is always >= 0 here: the initial setup
                    // only emits -1 alongside idx < 0 (which would
                    // skip the inner loop), and post-step updates
                    // assign `lines.len() - 1 >= 0`.
                    debug_assert!(end_line >= 0);
                    self.save_search_resume(
                        regex,
                        SearchDir::Backward,
                        idx as usize,
                        end_line.max(0) as usize,
                    );
                    return SearchOutcome::BudgetExhausted;
                }
                *budget -= 1;
                let lines = &self.records[idx as usize].lines;
                let upper = end_line.min(lines.len() as isize - 1);
                if upper >= 0
                    && let Some(hit) = (0..=upper as usize)
                        .rev()
                        .find(|&i| regex.is_match(&lines[i]))
                {
                    let key = self.records[idx as usize].key();
                    self.anchor = Anchor::On { key, line: hit };
                    self.ensure_window(engine, viewport_height);
                    return SearchOutcome::Found;
                }
                idx -= 1;
                if idx >= 0 {
                    end_line =
                        self.records[idx as usize].lines.len() as isize - 1;
                }
            }
            if self.backward_eof {
                return SearchOutcome::NotFound;
            }
            let fetched = self.extend_backward_batch(engine);
            if fetched == 0 {
                continue;
            }
            self.trim_back();
            // Newly prepended records sit at indices [0, fetched);
            // continue scanning from the most recent of them backward
            // toward index 0.
            idx = fetched as isize - 1;
            end_line = self.records[idx as usize].lines.len() as isize - 1;
        }
    }

    /// Moves a record-granularity selection by `delta` cached records
    /// and returns the new selection key (or `None` if there are no
    /// cached records).  Extends the window when the move would walk
    /// past an edge.
    pub fn move_selection(
        &mut self,
        engine: &Engine,
        current: &RecordKey,
        delta: isize,
        viewport_height: u16,
    ) -> Option<RecordKey> {
        let _ = viewport_height;
        let mut idx = self.find_record_idx(current)? as isize;
        let mut target = idx + delta;
        // Extend forward to make `target` representable.
        while target >= self.records.len() as isize && !self.forward_eof
        {
            self.extend_forward_batch(engine);
            self.trim_front();
            idx = self.find_record_idx(current).map(|i| i as isize)?;
            target = idx + delta;
        }
        // Extend backward symmetrically.
        while target < 0 && !self.backward_eof {
            self.extend_backward_batch(engine);
            self.trim_back();
            idx = self.find_record_idx(current).map(|i| i as isize)?;
            target = idx + delta;
        }
        let last = self.records.len() as isize - 1;
        let clamped = target.clamp(0, last) as usize;
        Some(self.records[clamped].key())
    }

    /// Yields up to `viewport_height` display lines starting at the
    /// viewport's anchor.  Each yielded line carries the [`RecordKey`]
    /// of its source record (so the renderer can apply selection
    /// highlighting), and a flag indicating whether the line is the
    /// header line for that record.
    pub fn rendered_lines(
        &self,
        viewport_height: u16,
    ) -> Vec<RenderedLine<'_>> {
        let mut out = Vec::with_capacity(viewport_height as usize);
        if self.records.is_empty() {
            return out;
        }
        let (mut idx, mut line) = self.anchor_indices();
        let height = viewport_height as usize;
        while out.len() < height && idx < self.records.len() {
            let entry = &self.records[idx];
            while out.len() < height && line < entry.lines.len() {
                out.push(RenderedLine {
                    text: &entry.lines[line],
                    record: &entry.record,
                    is_header: line == 0,
                });
                line += 1;
            }
            idx += 1;
            line = 0;
        }
        out
    }
}

/// One rendered line for the viewport, borrowed from the StreamView's
/// internal storage.
pub struct RenderedLine<'a> {
    pub text: &'a str,
    pub record: &'a MergeRecord,
    pub is_header: bool,
}

fn total_lines(records: &VecDeque<WindowEntry>) -> usize {
    records.iter().map(|e| e.lines.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::test_util::{TestDir, append_bunyan_at, append_raw, t};
    use camino::Utf8Path;

    fn write_fixture(path: &Utf8Path, name: &str, secs: &[i64]) {
        for s in secs {
            append_bunyan_at(path, name, t(*s), &format!("m{s}"));
        }
    }

    fn build_engine(paths: &[(&str, &[i64])], dir: &TestDir) -> Engine {
        let mut engine = Engine::new();
        for (name, secs) in paths {
            let p = dir.path().join(format!("{name}.log"));
            write_fixture(&p, name, secs);
            engine.add_file_source(&p).unwrap();
        }
        engine
    }

    fn anchor_msg(view: &StreamView) -> Option<String> {
        view.anchor_record().and_then(|r| match &r.event {
            Ok(e) => Some(e.msg.clone()),
            Err(_) => None,
        })
    }

    #[test]
    fn empty_engine_produces_empty_view() {
        let engine = Engine::new();
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        assert!(view.is_empty());
        assert!(view.anchor_record().is_none());
        assert!(view.rendered_lines(20).is_empty());
    }

    #[test]
    fn ensure_window_populates_from_default_cursor() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        assert_eq!(view.record_count(), 3);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m10"));
        let rendered: Vec<&str> =
            view.rendered_lines(20).iter().map(|l| l.text).collect();
        assert_eq!(rendered.len(), 3);
        assert!(rendered[0].contains("m10"));
        assert!(rendered[1].contains("m20"));
        assert!(rendered[2].contains("m30"));
        dir.cleanup();
    }

    #[test]
    fn seek_to_end_anchors_on_last_record() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.seek_to_end(&engine, 20).unwrap();
        assert_eq!(anchor_msg(&view).as_deref(), Some("m30"));
        // Forward EOF; backward walk replays records.
        assert!(view.record_count() >= 1);
        dir.cleanup();
    }

    #[test]
    fn scroll_lines_advances_anchor_through_records() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30, 40])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m10"));
        view.scroll_lines(&engine, 1, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        view.scroll_lines(&engine, 2, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m40"));
        // Scroll past EOF clamps to the last record.
        view.scroll_lines(&engine, 5, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m40"));
        // Scroll backward 2 records.
        view.scroll_lines(&engine, -2, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        // Scroll past start clamps to first.
        view.scroll_lines(&engine, -10, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m10"));
        dir.cleanup();
    }

    #[test]
    fn scroll_extends_forward_when_past_window() {
        // Force the initial fetch to be small by using a tiny over-fetch
        // implicitly via small viewport height.  The first ensure_window
        // fetches BATCH_SIZE records (64), so any modest fixture will
        // fit; for this test we just verify scroll past the cached set
        // triggers more fetching.
        let dir = TestDir::new();
        let n = (BATCH_SIZE * 2 + 5) as i64;
        let secs: Vec<i64> = (0..n).collect();
        let engine = build_engine(&[("a", &secs)], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 5);
        // Initial window is BATCH_SIZE records.
        let initial = view.record_count();
        // Scroll past the initial window.
        for _ in 0..initial + 10 {
            view.scroll_lines(&engine, 1, 5);
        }
        // We should have fetched more.  Anchor is somewhere past the
        // initial cap.
        assert!(view.record_count() > initial || view.forward_eof);
        dir.cleanup();
    }

    #[test]
    fn set_filter_resets_to_top_with_new_filter() {
        let dir = TestDir::new();
        let engine = build_engine(
            &[("nexus", &[10, 20, 30]), ("sled", &[15, 25, 35])],
            &dir,
        );
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        assert_eq!(view.record_count(), 6);
        // Apply a name filter - tighten to nexus only.
        let filter: Filter = "name=nexus".parse().unwrap();
        view.set_filter(filter);
        view.ensure_window(&engine, 20);
        assert_eq!(view.record_count(), 3);
        let rendered: Vec<&str> =
            view.rendered_lines(20).iter().map(|l| l.text).collect();
        assert!(rendered[0].contains("m10"));
        assert!(rendered[1].contains("m20"));
        assert!(rendered[2].contains("m30"));
        dir.cleanup();
    }

    #[test]
    fn set_show_extras_keeps_anchor_on_same_record() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        view.scroll_lines(&engine, 1, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        let mut o = view.render_opts();
        o.show_extras = true;
        view.set_render_opts(o);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        o.show_extras = false;
        view.set_render_opts(o);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        dir.cleanup();
    }

    #[test]
    fn set_show_date_reformats_window_timestamps() {
        // Toggling `show_date` should rewrite each cached entry's
        // pre-formatted lines so the next render reflects the new
        // setting.  No anchor or line-count change is expected: only
        // the leading timestamp shrinks.
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let dated_first =
            view.records().next().map(|(_, lines)| lines[0].to_string()).unwrap();
        assert!(
            dated_first.starts_with("1970-01-01T00:00:10.000Z "),
            "expected dated header, got {dated_first:?}",
        );

        let mut o = view.render_opts();
        o.show_date = false;
        view.set_render_opts(o);
        assert!(!view.render_opts().show_date);
        let undated_first =
            view.records().next().map(|(_, lines)| lines[0].to_string()).unwrap();
        assert!(
            undated_first.starts_with("00:00:10.000Z "),
            "expected time-only header after toggle, got {undated_first:?}",
        );

        // Idempotent: calling again with the current value is a no-op.
        view.set_render_opts(o);
        assert!(!view.render_opts().show_date);
        dir.cleanup();
    }

    #[test]
    fn advance_time_jumps_to_target_record() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30, 40, 50])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        view.advance_time(&engine, Duration::seconds(15), 20);
        // Anchor was at t=10; +15s = t=25, first event at or past
        // t=25 is t=30.
        assert_eq!(anchor_msg(&view).as_deref(), Some("m30"));
        view.advance_time(&engine, Duration::seconds(-5), 20);
        // From t=30, -5s = t=25, latest event at or before is t=20.
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        dir.cleanup();
    }

    /// Helper: a no-op `cancel` callback for tests that don't exercise
    /// the cancellation path.  Returns `false` every time, so the scan
    /// runs to whichever of [`SearchOutcome::Found`],
    /// [`SearchOutcome::NotFound`], or [`SearchOutcome::BudgetExhausted`]
    /// it would have reached.
    fn never_cancel() -> impl FnMut() -> bool {
        || false
    }

    #[test]
    fn search_step_finds_match_in_window() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let regex = Regex::new("m20").unwrap();
        let outcome = view.search_step(
            &engine,
            &regex,
            SearchDir::Forward,
            false,
            20,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::Found);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        dir.cleanup();
    }

    #[test]
    fn search_step_returns_not_found_when_no_match() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let regex = Regex::new("nonexistent").unwrap();
        let outcome = view.search_step(
            &engine,
            &regex,
            SearchDir::Forward,
            false,
            20,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::NotFound);
        dir.cleanup();
    }

    #[test]
    fn search_step_exclusive_skips_current_match() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30, 20, 40])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let regex = Regex::new("m20").unwrap();
        // First match: m20 at idx 1.
        let _ = view.search_step(
            &engine,
            &regex,
            SearchDir::Forward,
            false,
            20,
            &mut never_cancel(),
        );
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        // Next match (exclusive=true): the second m20 at idx 3.
        let outcome = view.search_step(
            &engine,
            &regex,
            SearchDir::Forward,
            true,
            20,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::Found);
        // Both records have msg "m20"; we can't distinguish by message
        // alone, but the offset must differ.
        let key = view.anchor_position().unwrap().0;
        assert_ne!(key.offset, ByteOffset::ZERO);
        dir.cleanup();
    }

    #[test]
    fn search_step_backward_walks_in_reverse() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30, 40, 50])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        // Move anchor to last record.
        view.scroll_lines(&engine, 4, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m50"));
        let regex = Regex::new("m20").unwrap();
        let outcome = view.search_step(
            &engine,
            &regex,
            SearchDir::Backward,
            true,
            20,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::Found);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        dir.cleanup();
    }

    #[test]
    fn search_step_resumes_after_budget_exhausted() {
        let dir = TestDir::new();
        // Five records; the match is the last.  A budget of 2 forces
        // BudgetExhausted on the first call (records 0 and 1 scanned)
        // and Found on the second.
        let engine =
            build_engine(&[("a", &[10, 20, 30, 40, 50])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let regex = Regex::new("m50").unwrap();
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            false,
            20,
            2,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::BudgetExhausted);
        // Anchor unchanged from initial position.
        assert_eq!(anchor_msg(&view).as_deref(), Some("m10"));
        // Resume picks up from record 2 with the same regex/direction.
        // A budget of 5 is plenty to finish.
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            false,
            20,
            5,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::Found);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m50"));
        dir.cleanup();
    }

    #[test]
    fn search_step_resume_invalidated_by_anchor_move() {
        let dir = TestDir::new();
        let engine =
            build_engine(&[("a", &[10, 20, 30, 40, 50])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let regex = Regex::new("nonexistent").unwrap();
        // Exhaust the budget without finding anything.
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            false,
            20,
            2,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::BudgetExhausted);
        // User scrolls — the resume is now stale.
        view.scroll_lines(&engine, 1, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        // Searching with a generous budget completes from the new
        // anchor and reports NotFound (not BudgetExhausted on the same
        // prefix).
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            false,
            20,
            100,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::NotFound);
        dir.cleanup();
    }

    #[test]
    fn search_step_resume_invalidated_by_pattern_change() {
        let dir = TestDir::new();
        let engine =
            build_engine(&[("a", &[10, 20, 30, 40, 50])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let r1 = Regex::new("nonexistent").unwrap();
        let outcome = view.search_step_with_budget(
            &engine,
            &r1,
            SearchDir::Forward,
            false,
            20,
            2,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::BudgetExhausted);
        // Different regex: the resume should be dropped and the new
        // search starts from the anchor.  Match m20 sits at record 1,
        // well within a budget of 2 starting from record 0.
        let r2 = Regex::new("m20").unwrap();
        let outcome = view.search_step_with_budget(
            &engine,
            &r2,
            SearchDir::Forward,
            false,
            20,
            2,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::Found);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        dir.cleanup();
    }

    #[test]
    fn search_step_found_clears_resume() {
        // After a Found, the next search should start from the new
        // anchor (not from a leftover resume point).  We assert this
        // indirectly: search exhausts → finds → search again with the
        // same regex/dir/budget and a too-tight budget should now hit
        // a fresh BudgetExhausted relative to the new anchor.
        let dir = TestDir::new();
        let engine =
            build_engine(&[("a", &[10, 20, 30, 40, 50])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let regex = Regex::new("m30").unwrap();
        // Find m30 (record 2) within a generous budget.
        let _ = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            false,
            20,
            10,
            &mut never_cancel(),
        );
        assert_eq!(anchor_msg(&view).as_deref(), Some("m30"));
        // Now search exclusively (n) for the SAME pattern with a
        // budget of 1 from the anchor at m30.  No more m30 ahead, so
        // it'll budget-exhaust before reaching the (nonexistent) next
        // match — i.e., on record 3 (m40).  If the resume from a
        // prior Found weren't cleared, this call would resume past
        // record 2 with stale state and might report different
        // behavior.
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            true,
            20,
            1,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::BudgetExhausted);
        // The anchor stayed on m30 (BudgetExhausted doesn't move it).
        assert_eq!(anchor_msg(&view).as_deref(), Some("m30"));
        dir.cleanup();
    }

    #[test]
    fn search_step_cancelled_leaves_anchor_and_resume_untouched() {
        // The cancel callback fires on the very first record check, so
        // the scan returns Cancelled before touching the anchor.  A
        // follow-up search with `never_cancel` then completes normally
        // — proving no stale resume point lingered from the cancelled
        // run.
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let regex = Regex::new("m30").unwrap();
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            false,
            20,
            10,
            &mut || true,
        );
        assert_eq!(outcome, SearchOutcome::Cancelled);
        // Anchor pinned at the start (m10), where it began.
        assert_eq!(anchor_msg(&view).as_deref(), Some("m10"));
        // Re-running without cancellation finds m30 from the original
        // anchor — a saved resume point would have skipped it.
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            false,
            20,
            10,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::Found);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m30"));
        dir.cleanup();
    }

    #[test]
    fn parse_errors_appear_in_rendered_lines() {
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan_at(&p, "x", t(10), "m10");
        append_raw(&p, "not json");
        append_bunyan_at(&p, "x", t(20), "m20");
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        assert_eq!(view.record_count(), 3);
        let rendered: Vec<&str> =
            view.rendered_lines(20).iter().map(|l| l.text).collect();
        assert!(rendered[0].contains("m10"));
        assert!(
            rendered[1].contains("not json")
                || rendered[1].contains("parse")
                || rendered[1].contains("error"),
        );
        assert!(rendered[2].contains("m20"));
        dir.cleanup();
    }

    #[test]
    fn seek_to_cursor_lands_at_specified_position() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30, 40])], &dir);
        // Walk forward 2 records from the start to get a cursor past
        // m10 and m20.
        let mut stepper = engine.stepper(Filter::default(), &Cursor::new());
        let _ = stepper.step_forward();
        let _ = stepper.step_forward();
        let mid_cursor = stepper.cursor();

        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.seek_to_cursor(&engine, mid_cursor, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m30"));
        dir.cleanup();
    }

    #[test]
    fn seek_to_cursor_falls_back_backward_when_forward_excluded() {
        // Build a 4-record stream and walk to a cursor just past the
        // second record.  Then seek under a filter that excludes
        // every record at or after that cursor; the seek should
        // rewind to the most recent visible record before the cursor
        // (m20) rather than yielding an empty view.
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30, 40])], &dir);
        let mut stepper = engine.stepper(Filter::default(), &Cursor::new());
        let _ = stepper.step_forward(); // past m10
        let _ = stepper.step_forward(); // past m20
        let mid_cursor = stepper.cursor();
        // Filter hides everything at or after the cursor (m30, m40).
        let filter: Filter = "msg!=m30 msg!=m40".parse().unwrap();
        let mut view = StreamView::new(filter, RenderOpts::default());
        view.seek_to_cursor(&engine, mid_cursor, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        dir.cleanup();
    }

    #[test]
    fn seek_to_cursor_yields_empty_when_no_record_passes_filter() {
        // With nothing visible in either direction, seek_to_cursor
        // ends in an empty view rather than looping or panicking.
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30])], &dir);
        let cursor = Cursor::new();
        let filter: Filter = "msg!=m10 msg!=m20 msg!=m30".parse().unwrap();
        let mut view = StreamView::new(filter, RenderOpts::default());
        view.seek_to_cursor(&engine, cursor, 20);
        assert!(view.is_empty());
        dir.cleanup();
    }

    #[test]
    fn cursor_before_record_round_trips_through_seek() {
        // The cursor returned for record `i` should land a stepper
        // exactly on record `i` when stepped forward — i.e., it's "just
        // before".  Drive that round-trip through `seek_to_cursor` and
        // verify the anchor lands on the expected record.
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30, 40])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        // First record's cursor is `front_cursor` itself.
        let c0 = view.cursor_before_record(0).unwrap();
        let mut v0 =
            StreamView::new(Filter::default(), RenderOpts::default());
        v0.seek_to_cursor(&engine, c0, 20);
        assert_eq!(anchor_msg(&v0).as_deref(), Some("m10"));
        // A middle record: walks past two preceding entries.
        let c2 = view.cursor_before_record(2).unwrap();
        let mut v2 =
            StreamView::new(Filter::default(), RenderOpts::default());
        v2.seek_to_cursor(&engine, c2, 20);
        assert_eq!(anchor_msg(&v2).as_deref(), Some("m30"));
        // Past the end is a clean None.
        assert!(view.cursor_before_record(view.record_count()).is_none());
        dir.cleanup();
    }

    #[test]
    fn cursor_before_record_handles_multiple_sources() {
        // With two interleaved sources, the cursor for a record from
        // source B must capture A's "just past last seen" offset too —
        // otherwise a forward step from the cursor would re-emit A's
        // earlier records.
        let dir = TestDir::new();
        let engine = build_engine(
            &[("a", &[10, 30, 50]), ("b", &[20, 40, 60])],
            &dir,
        );
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        // Records are interleaved: m10(a), m20(b), m30(a), m40(b), ...
        // The cursor before index 3 (m40 from b) should land us on m40.
        let cursor = view.cursor_before_record(3).unwrap();
        let mut v = StreamView::new(Filter::default(), RenderOpts::default());
        v.seek_to_cursor(&engine, cursor, 20);
        assert_eq!(anchor_msg(&v).as_deref(), Some("m40"));
        dir.cleanup();
    }

    #[test]
    fn rendered_lines_caps_at_viewport_height() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30, 40, 50])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let lines = view.rendered_lines(3);
        assert_eq!(lines.len(), 3);
        let lines = view.rendered_lines(100);
        // Capped at total available.
        assert_eq!(lines.len(), 5);
        dir.cleanup();
    }

    #[test]
    fn scroll_then_set_filter_resets_to_top() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        view.scroll_lines(&engine, 2, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m30"));
        view.set_filter(Filter::default());
        view.ensure_window(&engine, 20);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m10"));
        dir.cleanup();
    }
}
