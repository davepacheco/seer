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
//! [`FETCH_BATCH_SIZE`]-sized batch, which is small relative to the
//! parse cost.
//!
//! Summary tabs do not use `StreamView`; they keep the existing
//! full-pass model since their output is bounded by the histogram
//! shape, not by the file size.

use crate::Stepper;
use crate::engine::{
    Cursor, Engine, EngineEvent, FETCH_BATCH_SIZE, MergeRecord, StepperOptions,
};
use crate::event::Event;
use crate::filter::Filter;
use crate::position::{ByteLen, ByteOffset, LogStreamPosition, SourceId};
use crate::render::{RenderOpts, format_event};
use crate::source::Direction;
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration as StdDuration, Instant};

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
        Self { source_id: record.source_id().clone(), offset: record.offset() }
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

/// One materialized record in a [`Materialized`].  Either a parsed
/// event (the common case) or an error from the merge layer with its
/// `Display` form preserved.
///
/// The `Error` variant carries the stringified error so callers don't
/// have to look up `formatted[first_line_for_event[i]]` to recover
/// it — the pairing is enforced at the type level.
#[derive(Debug, Clone)]
pub enum Row {
    Event(EngineEvent),
    /// `Display` form of the underlying [`crate::engine::MergeError`].
    /// The same string is cached at
    /// `formatted[first_line_for_event[i]]` for the render path; this
    /// copy is the source of truth.  Carried (rather than a bare unit
    /// variant) so a [`Row::Error`] slot can't exist without its
    /// message attached.  Not yet read by any consumer; expected first
    /// reader is a future "show error details" dialog or a refined
    /// exclude-mode notice.
    #[allow(dead_code)]
    Error(String),
}

/// Index of a record within a [`Materialized::events`] vector.
///
/// Distinct from [`LineIdx`] (a position in `formatted`).  Keeping
/// them as separate types makes accidental swaps between the two
/// index domains — most importantly the `event_for_line` /
/// `first_line_for_event` translation tables — a compile error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventIdx(pub usize);

impl EventIdx {
    pub const ZERO: Self = Self(0);

    pub fn get(self) -> usize {
        self.0
    }
}

impl std::ops::Add<usize> for EventIdx {
    type Output = Self;

    fn add(self, rhs: usize) -> Self {
        Self(self.0 + rhs)
    }
}

impl std::fmt::Display for EventIdx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl PartialEq<usize> for EventIdx {
    fn eq(&self, other: &usize) -> bool {
        self.0 == *other
    }
}

impl PartialOrd<usize> for EventIdx {
    fn partial_cmp(&self, other: &usize) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(other)
    }
}

/// Index of a display row within a [`Materialized::formatted`] vector.
///
/// See [`EventIdx`] for the type-safety rationale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineIdx(pub usize);

impl LineIdx {
    pub const ZERO: Self = Self(0);

    pub fn get(self) -> usize {
        self.0
    }

    pub fn saturating_sub(self, n: usize) -> Self {
        Self(self.0.saturating_sub(n))
    }
}

impl std::ops::Add<usize> for LineIdx {
    type Output = Self;

    fn add(self, rhs: usize) -> Self {
        Self(self.0 + rhs)
    }
}

impl std::ops::AddAssign<usize> for LineIdx {
    fn add_assign(&mut self, rhs: usize) {
        self.0 += rhs;
    }
}

impl std::ops::Sub<LineIdx> for LineIdx {
    type Output = usize;

    fn sub(self, rhs: LineIdx) -> usize {
        self.0 - rhs.0
    }
}

impl std::fmt::Display for LineIdx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl PartialEq<usize> for LineIdx {
    fn eq(&self, other: &usize) -> bool {
        self.0 == *other
    }
}

impl PartialOrd<usize> for LineIdx {
    fn partial_cmp(&self, other: &usize) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(other)
    }
}

/// Flat materialization of a [`StreamView`]'s current window.
///
/// `events` is one entry per record produced by the underlying merge:
/// [`Row::Event`] for an `Ok` result, [`Row::Error`] for a parse / I/O
/// error or out-of-order warning.  `formatted` is one entry per
/// *display line* — an event with `n` extra fields contributes `1 + n`
/// lines (header plus indented `key = value` rows), and an error
/// contributes a single line carrying its `Display` message.
/// `formatted.len() >= events.len()`, with equality only when no
/// event has any extras.
///
/// `event_for_line[i]` is the index into `events` of the record that
/// produced display line `i`.  `first_line_for_event[i]` is the
/// inverse: the first line index for record `i`.  Together they let
/// callers translate freely between "scroll position" (a line index)
/// and "the record under the cursor" (an event index) without
/// rescanning.
///
/// Built by [`StreamView`] and refreshed in-place at the end of every
/// mutator that affects the window or its formatting.  Test fixtures
/// that don't go through `StreamView` (e.g. summary placeholders, or
/// the TUI's `App::with_events` synthetic-tab path) build a
/// `Materialized` directly via [`Materialized::synthetic`].
#[derive(Debug, Clone, Default)]
pub struct Materialized {
    pub events: Vec<Row>,
    pub formatted: Vec<String>,
    pub event_for_line: Vec<EventIdx>,
    pub first_line_for_event: Vec<LineIdx>,
    pub parse_stats: ParseStats,
}

impl Materialized {
    pub fn new(expected_records: usize) -> Materialized {
        Materialized {
            events: Vec::with_capacity(expected_records),
            formatted: Vec::new(),
            event_for_line: Vec::new(),
            first_line_for_event: Vec::with_capacity(expected_records),
            parse_stats: Default::default(),
        }
    }
}

/// Renders a [`MergeRecord`] into one or more display lines.
///
/// When `opts.show_raw` is set, the record's raw bytes are returned
/// verbatim (one line per record); otherwise `Ok` events use
/// [`format_event`] (header plus extras) and `Err` events produce a
/// single line carrying the [`MergeError`]'s `Display` message,
/// matching the existing TUI behavior of surfacing parse errors inline
/// next to events.
fn format_record(record: &MergeRecord, opts: &RenderOpts) -> Vec<String> {
    if opts.show_raw {
        // Synthetic error placeholders (e.g. an I/O failure from the
        // source) have empty `raw`; fall back to the error's Display
        // so the row still says something meaningful.
        if record.raw().is_empty()
            && let Err(err) = record.event()
        {
            return vec![err.to_string()];
        }
        return vec![record.raw().to_string()];
    }
    match record.event() {
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
    // XXX-dap line should be LineIdx
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
/// adds the records it pulled, the bytes it walked off disk, and the
/// wall-clock time spent.
#[derive(Clone, Debug, Default)]
pub struct ParseStats {
    /// Records appended to the window (i.e., records that passed the
    /// filter).  Equals the number of filter-matching records read.
    pub records: u64,
    /// Total bytes scanned off disk while populating the window —
    /// including bytes from records the filter rejected.  Under a
    /// selective filter this can be many orders of magnitude larger
    /// than the size of the matching records; it's what the TUI's
    /// status line and long-op progress bar both read, so each number
    /// reflects the work the engine actually had to do.
    pub walked_bytes: ByteLen,
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

/// Whether the search anchor (the row the user is currently parked on)
/// counts as a candidate match.  `Include` is the natural starting
/// position; `Skip` is what `n` / `N` repeats want so the cursor
/// advances instead of re-landing on the row it's already on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchAnchor {
    Include,
    Skip,
}

/// Whether [`StreamView::ensure_window_step`] needs more batches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowFillStatus {
    /// More batches needed.  Caller should call again after rendering.
    NotDone,
    /// Window is fully populated and the anchor is resolved to a
    /// concrete record (or [`Anchor::Empty`] for filter-rejects-all).
    Done,
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

/// Tracks whether each scan direction has already exhausted the
/// engine's content under the active filter.  Set by the extension
/// methods when a scan returns `eof = true`; cleared when the filter
/// or window position changes invalidate the prior conclusion.
///
/// Pairing the two flags inside one type makes their relationship
/// explicit; today every caller knows its direction at compile time
/// and reads / writes `eof.forward` or `eof.backward` directly.  Add
/// `fn get(self, dir: Direction) -> bool` (matching on `dir`) if a
/// future caller has a [`Direction`] in hand.
#[derive(Clone, Copy, Debug, Default)]
struct DirectionalEof {
    forward: bool,
    backward: bool,
}

// XXX-dap rip me out!
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
    eof: DirectionalEof,
    anchor: Anchor,
    parse_stats: ParseStats,
    /// Set when a search returns [`SearchOutcome::BudgetExhausted`] so
    /// the next `search_step` call can pick up where this one stopped.
    /// Cleared on `Found` and `NotFound`; ignored when the regex,
    /// direction, or anchor changes between calls.
    search_resume: Option<SearchResumePoint>,
    /// Cached flat view of the window for the TUI render path,
    /// recomputed at the end of every mutator that touches the records
    /// or formatting.  Callers read via [`Self::materialized`] — there's
    /// only one source of truth for the flat shape, so the old
    /// "streamview + Tab carry parallel copies" sync hazard is gone.
    materialized: Materialized,
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
            eof: DirectionalEof::default(),
            anchor: Anchor::PinFront,
            parse_stats: ParseStats::default(),
            search_resume: None,
            materialized: Materialized::default(),
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

    /// Returns the cached flat materialization of the current window.
    /// Refreshed in-place at the end of every mutator that affects the
    /// records or formatting, so callers can read `&self` without
    /// triggering a recompute.
    pub fn materialized(&self) -> &Materialized {
        &self.materialized
    }

    /// Rebuilds [`Self::materialized`] from the current `records` deque
    /// and `parse_stats`.  Called from every mutator that slides the
    /// window, swaps render options, or otherwise changes what would
    /// be rendered.  O(window_size) per call; the window is bounded by
    /// [`WINDOW_SOFT_CAP`] so this is microseconds in practice.
    fn recompute_materialized(&mut self) {
        let mut events = Vec::with_capacity(self.records.len());
        let mut formatted = Vec::new();
        let mut event_for_line = Vec::new();
        let mut first_line_for_event = Vec::with_capacity(self.records.len());
        let mut ordinals: HashMap<(SourceId, DateTime<Utc>), u64> =
            HashMap::new();
        for entry in &self.records {
            let event_idx = EventIdx(events.len());
            first_line_for_event.push(LineIdx(formatted.len()));
            match entry.record.event() {
                Ok(event) => {
                    let key = (entry.record.source_id().clone(), event.time);
                    let ordinal = *ordinals.entry(key.clone()).or_insert(0);
                    ordinals.insert(key, ordinal + 1);
                    let position = LogStreamPosition::new(
                        entry.record.source_id().clone(),
                        event.time,
                        ordinal,
                    );
                    for line in &entry.lines {
                        formatted.push(line.clone());
                        event_for_line.push(event_idx);
                    }
                    events.push(Row::Event(EngineEvent {
                        position,
                        event: event.clone(),
                    }));
                }
                Err(err) => {
                    let msg = err.to_string();
                    formatted.push(msg.clone());
                    event_for_line.push(event_idx);
                    events.push(Row::Error(msg));
                }
            }
        }
        self.materialized = Materialized {
            events,
            formatted,
            event_for_line,
            first_line_for_event,
            parse_stats: self.parse_stats.clone(),
        };
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
                self.anchor = Anchor::On { key: entry.key(), line: remaining };
                return;
            }
            remaining -= entry.lines.len();
        }
        // Past end: clamp to last record's last line.
        let last = self.records.back().unwrap();
        self.anchor =
            Anchor::On { key: last.key(), line: last.lines.len() - 1 };
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

    fn find_record_idx(&self, key: &RecordKey) -> Option<usize> {
        self.records.iter().position(|e| {
            e.record.source_id() == &key.source_id
                && e.record.offset() == key.offset
        })
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
            cursor.set(r.source_id().clone(), r.offset() + r.length());
        }
        Some(cursor)
    }

    /// Returns true iff the window is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Returns true iff a forward scan from the back of the window has
    /// already exhausted every source under the active filter — no more
    /// records can appear past `records.back()`.  The TUI uses this to
    /// surface an "at end of stream" indicator when the viewport's
    /// bottom coincides with the last cached line.
    pub fn is_forward_eof(&self) -> bool {
        self.eof.forward
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
        // Both `show_extras` and `show_raw` can collapse or expand the
        // line count per record (raw flattens an `n+1`-line event into
        // 1, extras adds/removes the trailing `n`).  When either
        // changes, the anchor's exact line offset within the record
        // can't be preserved, so snap to the record's first line.
        let line_count_changed = opts.show_extras != self.opts.show_extras
            || opts.show_raw != self.opts.show_raw;
        self.opts = opts;
        self.reformat_window();
        if line_count_changed && let Anchor::On { line, .. } = &mut self.anchor
        {
            *line = 0;
        }
        self.recompute_materialized();
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

    /// Sets up the view for a forward fetch from the merged stream's
    /// beginning, but does not fetch anything.  Paired with
    /// [`Self::ensure_window_step`] when the caller wants to drive
    /// the population in chunks (e.g. behind a long-op progress bar).
    pub fn prepare_seek_to_start(&mut self) {
        self.records.clear();
        self.front_cursor = Cursor::new();
        self.back_cursor = Cursor::new();
        self.eof.forward = false;
        self.eof.backward = true;
        self.anchor = Anchor::PinFront;
        self.search_resume = None;
        self.recompute_materialized();
    }

    /// Sets up the view for a backward fetch from EOF, but does not
    /// fetch anything.  Paired with [`Self::ensure_window_step`] for
    /// the long-op-driven path.
    pub fn prepare_seek_to_end(
        &mut self,
        engine: &Engine,
    ) -> std::io::Result<()> {
        let end = engine.cursor_at_end()?;
        self.records.clear();
        self.front_cursor = end.clone();
        self.back_cursor = end;
        self.eof.forward = true;
        self.eof.backward = false;
        self.anchor = Anchor::PinBack;
        self.search_resume = None;
        self.recompute_materialized();
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
        self.prepare_seek_to_cursor(cursor);
        self.ensure_window(engine, viewport_height);
        // No record at or after the cursor passes the filter — try
        // backward.  We swap to PinBack semantics here rather than
        // calling `seek_to_end`, which would walk the whole stream
        // backwards from EOF; here we want the *closest* visible
        // record before `cursor`, which is at most a batch away.
        if self.records.is_empty() && !self.eof.backward {
            self.anchor = Anchor::PinBack;
            self.ensure_window(engine, viewport_height);
        }
    }

    /// Sets up the view for a forward fetch from `cursor`, but does
    /// not fetch anything.  Paired with [`Self::ensure_window_step`]
    /// for the long-op-driven path.  The caller is responsible for
    /// implementing the PinFront → PinBack fallback that
    /// [`Self::seek_to_cursor`] does inline, since the long-op driver
    /// has to interleave it with cancellation/progress reporting.
    pub fn prepare_seek_to_cursor(&mut self, cursor: Cursor) {
        self.records.clear();
        self.front_cursor = cursor.clone();
        self.back_cursor = cursor;
        self.eof.forward = false;
        self.eof.backward = false;
        self.anchor = Anchor::PinFront;
        self.search_resume = None;
        self.recompute_materialized();
    }

    /// Sets the anchor to [`Anchor::PinBack`] without touching the
    /// cursors or records — used by the long-op driver to switch
    /// directions on the `seek_to_cursor` fallback when a forward
    /// fetch came up empty.
    pub fn set_anchor_pin_back(&mut self) {
        self.anchor = Anchor::PinBack;
    }

    /// Drives one batch of the work `ensure_window` would do
    /// synchronously, then returns control so the caller can render
    /// (and check for cancellation) before the next batch.  Used by
    /// the long-op driver behind `g`/`G`/filter rebuild — each tick of
    /// `advance_long_op` calls this once, lets the frame render with a
    /// progress bar, and repeats until [`WindowFillStatus::Done`].
    ///
    /// Equivalent to calling [`Self::ensure_window`] in a loop, but
    /// without holding the UI hostage on selective filters that have
    /// to walk many on-disk records to find one match.
    pub fn ensure_window_step(
        &mut self,
        engine: &Engine,
        viewport_height: u16,
    ) -> WindowFillStatus {
        let target_lines = viewport_height as usize + OVER_FETCH_LINES;
        // Phase 1: initial population.  When the deque's empty or
        // under target, fetch a *single* matching record in the
        // anchor's preferred direction.  One match per call keeps each
        // tick proportional to one match's worth of records walked —
        // important under selective filters, where finding a batch's
        // worth of matches (FETCH_BATCH_SIZE = 64) can mean walking many
        // thousands of on-disk records and freezing the UI.  The
        // long-op driver calls us repeatedly within a time budget per
        // frame, so multiple matches still get fetched per tick on
        // non-selective filters.  Phase 2 below resolves
        // PinFront/PinBack to On once we're done filling.
        let need_more = self.records.is_empty()
            || total_lines(&self.records) < target_lines;
        if need_more {
            // Bounded fill: each tick walks a small, fixed slice of
            // the file.  `batch_size = max_matches = 1` keeps the
            // matched-record count low so we don't buffer up matches
            // we'll discard on the next tick (each fresh stepper
            // throws away its un-popped buf).  `max_records_to_scan` caps the
            // per-tick wall time: under a 0.1%-selective filter,
            // 4,000 records scanned ≈ 400 ms — enough that the per-fill setup
            // cost amortizes against real work (the per-call file-
            // open + scan_backward initialization dominates if we
            // pick a much smaller budget), but short enough that the
            // user sees the progress bar tick several times per
            // second.
            const LONG_OP_BATCH: usize = 1;
            const LONG_OP_RECORDS_TO_SCAN_PER_FILL: usize = 4_000;
            let dir_eof = match self.anchor {
                Anchor::PinBack => {
                    self.extend_backward_small_batch(
                        engine,
                        LONG_OP_BATCH,
                        LONG_OP_BATCH,
                        LONG_OP_RECORDS_TO_SCAN_PER_FILL,
                    );
                    self.eof.backward
                }
                Anchor::PinFront | Anchor::On { .. } => {
                    self.extend_forward_small_batch(
                        engine,
                        LONG_OP_BATCH,
                        LONG_OP_BATCH,
                        LONG_OP_RECORDS_TO_SCAN_PER_FILL,
                    );
                    self.eof.forward
                }
                Anchor::Empty => true,
            };
            let target_met = total_lines(&self.records) >= target_lines;
            if !target_met && !dir_eof {
                self.recompute_materialized();
                return WindowFillStatus::NotDone;
            }
        }
        // Phase 2: resolve a pinned anchor to a concrete record.
        match self.anchor {
            Anchor::PinBack => {
                self.anchor = match self.records.back() {
                    Some(entry) => Anchor::On {
                        key: entry.key(),
                        line: entry.lines.len().saturating_sub(1),
                    },
                    None => Anchor::Empty,
                };
            }
            Anchor::PinFront => {
                self.anchor = match self.records.front() {
                    Some(entry) => Anchor::On { key: entry.key(), line: 0 },
                    None => Anchor::Empty,
                };
            }
            Anchor::On { .. } | Anchor::Empty => {}
        }
        // Phase 3: forward look-ahead from the anchor.  This is the
        // tail of `ensure_window` and is typically a no-op for the
        // initial seek paths (anchor at front means lines past anchor
        // == target_lines already; anchor at back means forward_eof
        // and nothing to fetch).  Run it synchronously: when it does
        // do work, it's the same look-ahead `ensure_window` does and
        // is bounded by `target_lines`.
        let anchor = self.anchor.clone();
        self.extend_forward_until(engine, |records, _| {
            let anchor_idx = anchor_idx_in(records, &anchor).unwrap_or(0);
            records
                .iter()
                .skip(anchor_idx)
                .map(|e| e.lines.len())
                .sum::<usize>()
                >= target_lines
        });
        self.recompute_materialized();
        WindowFillStatus::Done
    }

    /// Ensures the window has enough records to render the viewport
    /// plus an over-fetch buffer in each direction.  Cheap when the
    /// window is already populated.
    ///
    /// Two passes: an initial-population pass that fetches in the
    /// anchor's preferred direction when the window is empty, followed
    /// by look-ahead/look-behind extensions that fetch around the
    /// anchor so the viewport can fill *from* the anchor's record.
    /// The look-ahead pass matters most after a forward search lands
    /// the anchor on a match near the cached window's tail: without
    /// it, [`Self::anchor_flat_line`] would exceed the TUI's
    /// `max_top`, and the TUI's clamp would pin `viewport_top` instead
    /// of tracking the anchor as the user scrolls forward.
    pub fn ensure_window(&mut self, engine: &Engine, viewport_height: u16) {
        let target_lines = viewport_height as usize + OVER_FETCH_LINES;
        // Initial population: fetch forward (or backward when pinned
        // to the back) until we have either enough lines or hit EOF.
        if self.records.is_empty() {
            match self.anchor {
                Anchor::PinBack => {
                    self.extend_backward_until(engine, |records, _| {
                        total_lines(records) >= target_lines
                    });
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
                    self.extend_forward_until(engine, |records, _| {
                        total_lines(records) >= target_lines
                    });
                    if let Some(entry) = self.records.front() {
                        self.anchor = Anchor::On { key: entry.key(), line: 0 };
                    } else {
                        self.anchor = Anchor::Empty;
                    }
                }
            }
        }
        if self.records.is_empty() {
            self.recompute_materialized();
            return;
        }
        // Look-ahead: fetch forward batches until at least
        // `target_lines` of content sits at or past the anchor.  Stops
        // at forward EOF or when the buffer hits the soft cap.  Clone
        // the anchor up front so the closure doesn't borrow `self`.
        //
        // No matching backward pass: bookmark navigation and similar
        // cursor-anchored seeks should land precisely at the requested
        // record, not silently widen to include earlier ones.
        // `scroll_lines` fetches backward on demand when the user
        // actually scrolls back, so the absence of a pre-fetched
        // backward buffer only costs one extra batch fetch on the
        // first `k`.
        let anchor = self.anchor.clone();
        self.extend_forward_until(engine, |records, _| {
            let anchor_idx = anchor_idx_in(records, &anchor).unwrap_or(0);
            records
                .iter()
                .skip(anchor_idx)
                .map(|e| e.lines.len())
                .sum::<usize>()
                >= target_lines
        });
        self.recompute_materialized();
    }

    /// Fetches up to `FETCH_BATCH_SIZE` records forward and appends
    /// them.  Returns the number actually fetched.
    fn extend_forward_batch(&mut self, engine: &Engine) -> usize {
        if self.eof.forward {
            return 0;
        }
        let started = Instant::now();
        let was_empty = self.records.is_empty();
        let mut stepper =
            engine.stepper(self.filter.clone(), &self.back_cursor);
        let mut fetched = 0;
        for _ in 0..FETCH_BATCH_SIZE {
            match stepper.step_forward() {
                Some(record) => {
                    fetched += 1;
                    self.records
                        .push_back(WindowEntry::new(record, &self.opts));
                }
                None => {
                    self.eof.forward = true;
                    break;
                }
            }
        }
        self.back_cursor = stepper.cursor();
        self.parse_stats.records += fetched as u64;
        self.parse_stats.walked_bytes += stepper.walked_bytes();
        self.parse_stats.elapsed += started.elapsed();
        // First fetch into an empty window: anchor `front_cursor` at
        // the actual byte position of `records[0]` rather than leaving
        // it at the (possibly far-earlier) seek point.  Without this,
        // a selective filter that skips the first chunk of every file
        // leaves the user-status line reporting "byte offset 0" even
        // though the first visible record is hundreds of bytes deep.
        if was_empty {
            self.anchor_front_cursor_to_first_record();
        }
        fetched
    }

    /// Updates `front_cursor` so it captures the byte position the
    /// stepper had to walk *past* in each source to surface
    /// `records.front()` as the first match, then rolls the source
    /// that record came from back to the record's own offset.
    ///
    /// Without the multi-source-aware basis, a filter that excludes
    /// every record in one source (e.g. a sled-hostname filter against
    /// a per-sled log file) would leave the user-status byte offset
    /// reading 0 — the visible record's source might genuinely sit at
    /// byte 0 in its own file, but the engine has scanned past every
    /// byte of the other source's file to confirm it had no matches.
    /// The stepper's post-batch [`super::merge::Stepper::cursor`]
    /// captures that walked-past state (the matching merge.rs
    /// position-advance covers the EOF-no-matches case), so we copy it
    /// in wholesale and then roll back only the one source.
    fn anchor_front_cursor_to_first_record(&mut self) {
        if let Some(first) = self.records.front() {
            let first_source = first.record.source_id().clone();
            let first_offset = first.record.offset();
            // `self.back_cursor` was just set to `stepper.cursor()` by
            // the caller; both extend_forward paths write it
            // immediately before invoking us.
            self.front_cursor = self.back_cursor.clone();
            self.front_cursor.set(first_source, first_offset);
        }
    }

    /// Like [`Self::extend_forward_batch`] but uses a stepper with a
    /// small per-fill batch size and a per-fill records-to-scan budget so each
    /// `query` call walks only a bounded number of on-disk records.
    /// Used by the long-op driver behind `G`/`g`/filter rebuild —
    /// under a selective filter, the default batch size (64 matches)
    /// can force a fill to walk thousands of records per call and
    /// freeze the UI for hundreds of milliseconds; the bounded
    /// variant keeps each tick responsive even when the filter
    /// rejects almost everything.  Returns the number of matches
    /// added to the window.
    fn extend_forward_small_batch(
        &mut self,
        engine: &Engine,
        batch_size: usize,
        max_matches: usize,
        max_records_to_scan_per_fill: usize,
    ) -> usize {
        if self.eof.forward || max_matches == 0 {
            return 0;
        }
        let started = Instant::now();
        let was_empty = self.records.is_empty();
        let mut stepper = engine.stepper_with(
            self.filter.clone(),
            &self.back_cursor,
            StepperOptions {
                batch_size,
                max_records_to_scan_per_fill: Some(
                    max_records_to_scan_per_fill,
                ),
            },
        );
        let mut fetched = 0;
        for _ in 0..max_matches {
            match stepper.step_forward() {
                Some(record) => {
                    fetched += 1;
                    self.records
                        .push_back(WindowEntry::new(record, &self.opts));
                }
                None => {
                    // `step_forward` returns `None` either at true
                    // forward EOF or when the budget expired before
                    // a match surfaced.  Only set our own EOF flag
                    // when every per-source window is genuinely
                    // exhausted; otherwise leave it clear so the
                    // next tick can resume scanning.
                    if stepper.is_exhausted(Direction::Forward) {
                        self.eof.forward = true;
                    }
                    break;
                }
            }
        }
        self.back_cursor = stepper.cursor();
        self.parse_stats.walked_bytes += stepper.walked_bytes();
        self.parse_stats.records += fetched as u64;
        self.parse_stats.elapsed += started.elapsed();
        // See `extend_forward_batch` for the rationale.
        if was_empty {
            self.anchor_front_cursor_to_first_record();
        }
        fetched
    }

    /// Fetches up to `FETCH_BATCH_SIZE` records backward and prepends
    /// them.  Returns the number actually fetched.
    fn extend_backward_batch(&mut self, engine: &Engine) -> usize {
        self.extend_backward_batch_n(engine, FETCH_BATCH_SIZE, FETCH_BATCH_SIZE)
    }

    /// Symmetric to [`Self::extend_forward_small_batch`].
    fn extend_backward_small_batch(
        &mut self,
        engine: &Engine,
        batch_size: usize,
        max_matches: usize,
        max_records_to_scan_per_fill: usize,
    ) -> usize {
        if self.eof.backward || max_matches == 0 {
            return 0;
        }
        let started = Instant::now();
        let mut stepper = engine.stepper_with(
            self.filter.clone(),
            &self.front_cursor,
            StepperOptions {
                batch_size,
                max_records_to_scan_per_fill: Some(
                    max_records_to_scan_per_fill,
                ),
            },
        );
        let mut fetched = 0;
        for _ in 0..max_matches {
            match stepper.step_backward() {
                Some(record) => {
                    fetched += 1;
                    self.records
                        .push_front(WindowEntry::new(record, &self.opts));
                }
                None => {
                    if stepper.is_exhausted(Direction::Backward) {
                        self.eof.backward = true;
                    }
                    break;
                }
            }
        }
        // Only advance `front_cursor` when we actually prepended
        // records.  A no-fetch backward step (the user pressed `k` at
        // the top of the stream) would otherwise overwrite the
        // carefully-anchored value our forward pass installed —
        // backward fills on filter-excluded sources walk down to 0
        // and reset their position, which would zero out the user-
        // status byte offset for no real navigation.
        if fetched > 0 {
            self.front_cursor = stepper.cursor();
        }
        self.parse_stats.walked_bytes += stepper.walked_bytes();
        self.parse_stats.records += fetched as u64;
        self.parse_stats.elapsed += started.elapsed();
        fetched
    }

    /// Backward counterpart of `extend_forward_*`.  `stepper_batch`
    /// controls the per-fill batch size handed to the storage layer;
    /// `max_matches` caps the number of records appended to the
    /// window in this call.
    fn extend_backward_batch_n(
        &mut self,
        engine: &Engine,
        stepper_batch: usize,
        max_matches: usize,
    ) -> usize {
        if self.eof.backward || max_matches == 0 {
            return 0;
        }
        let started = Instant::now();
        let mut stepper = engine.stepper_with(
            self.filter.clone(),
            &self.front_cursor,
            StepperOptions { batch_size: stepper_batch, ..Default::default() },
        );
        let mut fetched = 0;
        // step_backward returns records in reverse time order; we
        // push them to the front, so the deque stays sorted oldest
        // first.
        for _ in 0..max_matches {
            match stepper.step_backward() {
                Some(record) => {
                    fetched += 1;
                    self.records
                        .push_front(WindowEntry::new(record, &self.opts));
                }
                None => {
                    self.eof.backward = true;
                    break;
                }
            }
        }
        // See `extend_backward_small_batch` for why we don't update
        // `front_cursor` on a zero-fetch step.
        if fetched > 0 {
            self.front_cursor = stepper.cursor();
        }
        self.parse_stats.records += fetched as u64;
        self.parse_stats.walked_bytes += stepper.walked_bytes();
        self.parse_stats.elapsed += started.elapsed();
        fetched
    }

    /// Repeatedly extends forward in `FETCH_BATCH_SIZE` chunks until `done`
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
                entry.record.source_id().clone(),
                entry.record.offset() + entry.record.length(),
            );
            // Trimming exposes earlier territory for backward fetches.
            self.eof.backward = false;
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
                .set(entry.record.source_id().clone(), entry.record.offset());
            self.eof.forward = false;
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
            // No motion, but `ensure_window` above may have populated
            // the deque (and thus recomputed); nothing more to do.
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
                let step = usize::try_from(remaining)
                    .expect("remaining > 0 by outer branch");
                if step <= lines_left {
                    line += step;
                    break;
                }
                let consumed = isize::try_from(lines_left + 1).expect(
                    "lines per record fits in isize for any realistic log",
                );
                remaining -= consumed;
                if idx + 1 < self.records.len() {
                    idx += 1;
                    line = 0;
                } else if !self.eof.forward {
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
                // `remaining < 0` here; work in unsigned magnitudes
                // and the sign drops out.
                let step = remaining.unsigned_abs();
                if step <= line {
                    line -= step;
                    break;
                }
                let consumed = isize::try_from(line + 1).expect(
                    "lines per record fits in isize for any realistic log",
                );
                remaining += consumed;
                if idx > 0 {
                    idx -= 1;
                    line = self.records[idx].lines.len() - 1;
                } else if !self.eof.backward {
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
            self.anchor = Anchor::On { key: self.records[idx].key(), line };
        }
        self.anchor = Anchor::On { key: self.records[idx].key(), line };
        self.recompute_materialized();
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
            Direction::Forward
        } else {
            Direction::Backward
        };
        let Some(anchor_time) = self.anchor_event_time(dir) else {
            return;
        };
        let target = anchor_time + delta;
        match dir {
            Direction::Forward => {
                self.advance_time_forward(engine, target, viewport_height)
            }
            Direction::Backward => {
                self.advance_time_backward(engine, target, viewport_height)
            }
        }
        self.recompute_materialized();
    }

    /// Returns the timestamp of the closest event to the anchor in
    /// the requested direction (preferring same-direction; falling
    /// back to opposite).  None when no event is in the window.
    fn anchor_event_time(&self, dir: Direction) -> Option<DateTime<Utc>> {
        if self.records.is_empty() {
            return None;
        }
        let (anchor_idx, _) = self.anchor_indices();
        let event_time = |i: usize| -> Option<DateTime<Utc>> {
            self.records[i].record.event().as_ref().ok().map(|e: &Event| e.time)
        };
        match dir {
            Direction::Forward => (anchor_idx..self.records.len())
                .find_map(event_time)
                .or_else(|| (0..anchor_idx).rev().find_map(event_time)),
            Direction::Backward => {
                (0..=anchor_idx).rev().find_map(event_time).or_else(|| {
                    ((anchor_idx + 1)..self.records.len()).find_map(event_time)
                })
            }
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
                if let Ok(ev) = self.records[idx].record.event()
                    && ev.time >= target
                {
                    let key = self.records[idx].key();
                    self.anchor = Anchor::On { key, line: 0 };
                    self.ensure_window(engine, viewport_height);
                    return;
                }
                idx += 1;
            }
            if self.eof.forward {
                // Snap to the last record.
                let last = self.records.len() - 1;
                let key = self.records[last].key();
                self.anchor = Anchor::On {
                    key,
                    line: self.records[last].lines.len().saturating_sub(1),
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
                if let Ok(ev) = self.records[i].record.event()
                    && ev.time <= target
                {
                    let key = self.records[i].key();
                    self.anchor = Anchor::On { key, line: 0 };
                    self.ensure_window(engine, viewport_height);
                    return;
                }
            }
            if self.eof.backward {
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
    /// `anchor` controls whether the row the user is currently parked
    /// on counts as a match candidate.  [`SearchAnchor::Skip`] is what
    /// `n` after a previous match wants (so the cursor advances rather
    /// than re-landing); [`SearchAnchor::Include`] is what the initial
    /// `/<pattern>` wants (the cursor's current line is eligible).
    ///
    /// Walks at most `budget` records per call before returning
    /// [`SearchOutcome::BudgetExhausted`].  When that happens, the next
    /// call with the same regex, direction, and an unchanged anchor
    /// resumes from where this one stopped; switching regex or
    /// direction or moving the anchor (e.g. by scrolling) drops the
    /// resume point and restarts from the anchor.  The TUI's
    /// progress-bar driver uses this to run the scan in chunks small
    /// enough to interleave with frame draws and Ctrl-C polls.
    ///
    /// `cancel` is consulted once per scanned record; returning `true`
    /// aborts the scan with [`SearchOutcome::Cancelled`], leaving the
    /// anchor unchanged and saving no resume point.  Callers that have
    /// no cancellation source can pass `&mut || false`.
    #[allow(clippy::too_many_arguments)]
    pub fn search_step_with_budget(
        &mut self,
        engine: &Engine,
        regex: &Regex,
        direction: SearchDir,
        anchor: SearchAnchor,
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
        let outcome = match direction {
            SearchDir::Forward => self.search_step_forward(
                engine,
                regex,
                anchor,
                &mut budget,
                viewport_height,
                resume_idx,
                cancel,
            ),
            SearchDir::Backward => self.search_step_backward(
                engine,
                regex,
                anchor,
                &mut budget,
                viewport_height,
                resume_idx,
                cancel,
            ),
        };
        self.recompute_materialized();
        outcome
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
        anchor: SearchAnchor,
        budget: &mut usize,
        viewport_height: u16,
        resume: Option<(usize, usize)>,
        cancel: &mut dyn FnMut() -> bool,
    ) -> SearchOutcome {
        let (mut idx, mut start_line) = resume.unwrap_or_else(|| {
            let (anchor_idx, anchor_line) = self.anchor_indices();
            let start_line = match anchor {
                SearchAnchor::Skip => anchor_line + 1,
                SearchAnchor::Include => anchor_line,
            };
            (anchor_idx, start_line)
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
            if self.eof.forward {
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
        anchor: SearchAnchor,
        budget: &mut usize,
        viewport_height: u16,
        resume: Option<(usize, usize)>,
        cancel: &mut dyn FnMut() -> bool,
    ) -> SearchOutcome {
        let skip_anchor = matches!(anchor, SearchAnchor::Skip);
        // `idx` and `end_line` are isize so they can hold the -1
        // "exhausted" sentinel; every read through them at usize first
        // proves the value is non-negative.
        let cap_isize = |n: usize| {
            isize::try_from(n)
                .expect("record/line counts fit in isize for any realistic log")
        };
        let (mut idx, mut end_line): (isize, isize) = match resume {
            Some((i, line)) => (cap_isize(i), cap_isize(line)),
            None => {
                let (anchor_idx, anchor_line) = self.anchor_indices();
                let mut idx = cap_isize(anchor_idx);
                // Initial scan upper bound: the anchor's line, or one
                // before it when `Skip`.  When skipping and the anchor
                // is on line 0, skip the current record entirely and
                // start at the previous one.
                let end = if skip_anchor && anchor_line == 0 {
                    idx -= 1;
                    if idx < 0 {
                        -1
                    } else {
                        let idx_u = usize::try_from(idx)
                            .expect("idx >= 0 by surrounding if");
                        cap_isize(self.records[idx_u].lines.len()) - 1
                    }
                } else if skip_anchor {
                    cap_isize(anchor_line.saturating_sub(1))
                } else {
                    cap_isize(anchor_line)
                };
                (idx, end)
            }
        };
        loop {
            while idx >= 0 {
                if cancel() {
                    return SearchOutcome::Cancelled;
                }
                let idx_u = usize::try_from(idx)
                    .expect("idx >= 0 by inner-loop condition");
                if *budget == 0 {
                    // end_line is always >= 0 here: the initial setup
                    // only emits -1 alongside idx < 0 (which would
                    // skip the inner loop), and post-step updates
                    // assign `lines.len() - 1 >= 0`.
                    debug_assert!(end_line >= 0);
                    let end_u = usize::try_from(end_line.max(0))
                        .expect("clamped to >= 0 by .max(0)");
                    self.save_search_resume(
                        regex,
                        SearchDir::Backward,
                        idx_u,
                        end_u,
                    );
                    return SearchOutcome::BudgetExhausted;
                }
                *budget -= 1;
                let lines = &self.records[idx_u].lines;
                let upper = end_line.min(cap_isize(lines.len()) - 1);
                if upper >= 0
                    && let upper_u = usize::try_from(upper)
                        .expect("upper >= 0 by surrounding if")
                    && let Some(hit) =
                        (0..=upper_u).rev().find(|&i| regex.is_match(&lines[i]))
                {
                    let key = self.records[idx_u].key();
                    self.anchor = Anchor::On { key, line: hit };
                    self.ensure_window(engine, viewport_height);
                    return SearchOutcome::Found;
                }
                idx -= 1;
                if idx >= 0 {
                    let idx_u = usize::try_from(idx)
                        .expect("idx >= 0 by surrounding if");
                    end_line = cap_isize(self.records[idx_u].lines.len()) - 1;
                }
            }
            if self.eof.backward {
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
            idx = cap_isize(fetched) - 1;
            let idx_u =
                usize::try_from(idx).expect("fetched > 0 by `if fetched == 0`");
            end_line = cap_isize(self.records[idx_u].lines.len()) - 1;
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
        // Record counts come from `VecDeque::len` for an in-memory
        // window, so they comfortably fit in isize for any realistic
        // log.
        let cap_isize = |n: usize| {
            isize::try_from(n).expect("window record count fits in isize")
        };
        let mut idx = cap_isize(self.find_record_idx(current)?);
        let mut target = idx + delta;
        // Extend forward to make `target` representable.
        while target >= cap_isize(self.records.len()) && !self.eof.forward {
            self.extend_forward_batch(engine);
            self.trim_front();
            idx = cap_isize(self.find_record_idx(current)?);
            target = idx + delta;
        }
        // Extend backward symmetrically.
        while target < 0 && !self.eof.backward {
            self.extend_backward_batch(engine);
            self.trim_back();
            idx = cap_isize(self.find_record_idx(current)?);
            target = idx + delta;
        }
        let last = cap_isize(self.records.len()) - 1;
        let clamped = usize::try_from(target.clamp(0, last))
            .expect("clamped to [0, last] >= 0");
        Some(self.records[clamped].key())
    }
}

fn total_lines(records: &VecDeque<WindowEntry>) -> usize {
    records.iter().map(|e| e.lines.len()).sum()
}

/// Returns the index of the entry in `records` whose record matches
/// `anchor`'s key, or `None` if the anchor isn't `Anchor::On` or the
/// record isn't present.  Used by [`StreamView::ensure_window`]'s
/// look-ahead/look-behind closures, which run inside a `&mut self`
/// borrow and can't reach `self.find_record_idx`.
fn anchor_idx_in(
    records: &VecDeque<WindowEntry>,
    anchor: &Anchor,
) -> Option<usize> {
    let Anchor::On { key, .. } = anchor else {
        return None;
    };
    records.iter().position(|e| {
        e.record.source_id() == &key.source_id
            && e.record.offset() == key.offset
    })
}

// -----------------------------------------------------------------------------
// XXX-dap TODO move this to src/ui

pub struct Viewport {
    filter: Filter,
    render_options: RenderOpts,
    anchor: Anchor,
    anchor_cursor: Cursor,
    rendered: RenderedWindow,
    pending_seek: Option<SeekOperation>,
}

struct SeekOperation {
    stepper: Stepper,
    direction: Direction,
    stats: ParseStats,
    seek_to: SeekDestination,
}

enum SeekDestination {
    Cursor,
    Distance(usize),
    Time(DateTime<Utc>),
    Search(Regex),
}

#[derive(Debug)]
pub enum ViewportStatus<'a> {
    Idle,
    Seeking(&'a ParseStats),
    Populating,
}

impl Viewport {
    pub fn new(
        engine: &Engine,
        filter: Filter,
        render_options: RenderOpts,
    ) -> Viewport {
        let anchor = Anchor::PinFront;
        let anchor_cursor = Cursor::new();
        let stepper = engine.stepper_batched(filter.clone(), &anchor_cursor);
        let rendered = RenderedWindow::new(stepper, 0, 1024, render_options); // XXX-dap
        Viewport {
            filter,
            render_options,
            anchor,
            anchor_cursor,
            rendered,
            pending_seek: None,
        }
    }

    /// Returns a [`Cursor`] that, when fed back into
    /// [`Self::start_seek_to_cursor`], lands the viewport on the same record
    /// the anchor is currently on.
    ///
    /// Returns `None` when the window is empty or the anchor isn't pinned to a
    /// specific record.
    pub fn cursor_at_anchor(&self) -> Option<Cursor> {
        match &self.anchor {
            Anchor::On { .. } => Some(self.anchor_cursor.clone()),
            Anchor::Empty | Anchor::PinFront | Anchor::PinBack => None,
        }
    }

    /// Returns the [`Cursor`] "just before" the record at window index
    /// `idx` — i.e., a cursor such that `engine.stepper(filter, &cursor)`'s
    /// next `step_forward` (under the same filter the window was built with)
    /// returns that record.
    pub fn cursor_before_record(&self, _idx: EventIdx) -> Option<Cursor> {
        // XXX-dap how are we going to compute this
        // We only have two possible cursors to start with:
        // - the anchor cursor
        // - the cursor wherever the stepper is right now
        // I'm inclined to clone the cursor wherever the stepper is right now
        // and start walking in whichever direction looks closer to this event.
        // The only thing is that we might not be able to tell which direction
        // to go?
        todo!(); // XXX-dap implement me
    }

    pub fn status(&self) -> ViewportStatus<'_> {
        if let Some(seek) = &self.pending_seek {
            ViewportStatus::Seeking(&seek.stats)
        } else if !self.rendered.is_populated() {
            ViewportStatus::Populating
        } else {
            ViewportStatus::Idle
        }
    }

    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    pub fn materialized(&self) -> &Materialized {
        self.rendered.materialized()
    }

    pub fn is_forward_eof(&self) -> bool {
        self.rendered.is_forward_eof()
    }

    /// Do a bounded amount of work trying to populate the current rendered
    /// window
    pub fn populate_work(&mut self) {
        self.rendered.populate_work();
    }

    pub fn set_filter(&mut self, engine: &Engine, filter: Filter) {
        // XXX-dap need to adjust anchor, which might now be filtered out
        self.seek_interrupt();
        self.filter = filter.clone();
        // Re-seek to the cursor under the new filter.
        self.start_seek_to_cursor(engine, &self.anchor_cursor.clone());
    }

    pub fn set_render_options(&mut self, render_options: RenderOpts) {
        self.render_options = render_options;
        self.rendered.set_render_options(render_options);
    }

    // Seeking

    pub fn seek_interrupt(&mut self) {
        self.pending_seek = None;
    }

    // XXX-dap this implicitly assumes the viewport height is smaller than the
    // window size.  I guess we could pass that through to the point where we
    // create the window
    pub fn scroll_lines(&mut self, delta: isize) {
        let stepper =
            self.rendered.stepper_at_cursor_within_window(&self.anchor_cursor);
        let direction =
            if delta > 0 { Direction::Forward } else { Direction::Backward };
        self.start_seek(
            stepper,
            direction,
            SeekDestination::Distance(delta.unsigned_abs()),
        );
        // Try doing a unit of work.  In the common case, this is going to be
        // pretty quick.
        self.seek_work();
    }

    pub fn start_seek_by_time(
        &mut self,
        direction: Direction,
        delta: Duration,
    ) {
        let Some(record) = self.anchor_record() else {
            // If there are no records, there's nothing to do.
            return;
        };

        let Ok(event) = record.event() else {
            // If this was an error, we can't seek to a time from here.
            // We could walk ahead to the next valid record, but so can the
            // user.
            return;
        };

        let end_time = match direction {
            Direction::Forward => event.time + delta,
            Direction::Backward => event.time - delta,
        };

        let stepper =
            self.rendered.stepper_at_cursor_within_window(&self.anchor_cursor);
        self.start_seek(stepper, direction, SeekDestination::Time(end_time))
    }

    pub fn start_seek_for_search(
        &mut self,
        direction: Direction,
        regex: Regex,
    ) {
        let stepper =
            self.rendered.stepper_at_cursor_within_window(&self.anchor_cursor);
        self.start_seek(stepper, direction, SeekDestination::Search(regex));
    }

    pub fn start_seek_to_cursor(&mut self, engine: &Engine, cursor: &Cursor) {
        // A cursor seek is a little different from a typical one because we
        // (mostly) know exactly where we're going.  But we treat it like an
        // unbounded-seek anyway because in principle it's possible for it to
        // take a lot of work to get to the next matching record.
        //
        // Unlike the other seek operations, we create a fresh Stepper from the
        // engine here because we want to load this position directly rather
        // than stepping to it.  It could be very far away.
        let stepper = engine.stepper_batched(self.filter.clone(), cursor);

        // The direction is unused here.
        // XXX-dap TODO-cleanup better strong type safety
        self.start_seek(stepper, Direction::Forward, SeekDestination::Cursor);
    }

    pub fn start_seek_to_start(&mut self, engine: &Engine) {
        self.start_seek_to_cursor(engine, &Cursor::new());
    }

    pub fn start_seek_to_end(&mut self, engine: &Engine) {
        // XXX-dap do we need to position the anchor specially?
        // XXX-dap do we need to do something to step back a bit from where we
        // ended?
        // XXX-dap unwrap(): source should store size instead of fetching each
        // time
        self.start_seek_to_cursor(engine, &engine.cursor_at_end().unwrap());
    }

    fn anchor_record(&self) -> Option<&MergeRecord> {
        match &self.anchor {
            Anchor::On { key, line: _ } => self.rendered.record_for_key(key),
            Anchor::Empty => None,
            Anchor::PinFront => self.rendered.record_first(),
            Anchor::PinBack => self.rendered.record_last(),
        }
    }

    fn start_seek(
        &mut self,
        stepper: Stepper,
        direction: Direction,
        seek_to: SeekDestination,
    ) {
        // XXX-dap, if we're pinned to the back, this seems like it won't do
        // what we want
        self.seek_interrupt();
        self.pending_seek = Some(SeekOperation {
            stepper,
            direction,
            stats: Default::default(),
            seek_to,
        });
    }

    pub fn seek_work(&mut self) {
        let Some(seek) = &mut self.pending_seek else {
            return;
        };

        // XXX-dap this could end up doing very little work
        let Some(next) = seek.stepper.step(seek.direction) else {
            if seek.stepper.is_exhausted(seek.direction) {
                self.seek_finish(None);
            }

            return;
        };

        match &mut seek.seek_to {
            SeekDestination::Cursor => {
                self.seek_finish(Some((next, LineIdx(0))));
            }
            SeekDestination::Distance(remaining) => {
                // Render the record to figure out how many lines it takes up.
                let nlines = format_record(&next, &self.render_options).len();
                if *remaining > nlines {
                    *remaining -= nlines;
                    return;
                }

                let value = Some((next, LineIdx(*remaining)));
                self.seek_finish(value);
            }
            SeekDestination::Time(target_time) => {
                let Ok(event) = next.event() else {
                    return;
                };

                let done = match seek.direction {
                    Direction::Forward => event.time >= *target_time,
                    Direction::Backward => event.time <= *target_time,
                };

                if done {
                    self.seek_finish(Some((next, LineIdx(0))));
                }
            }
            SeekDestination::Search(regex) => {
                // Render the record so we can see if the regex matches any of
                // the rendered text.
                // XXX-dap is this what it was doing before?  what if it gets
                // line-wrapped?
                let lines = format_record(&next, &self.render_options);
                for line in lines {
                    if regex.is_match(&line) {
                        self.seek_finish(Some((next, LineIdx(0))));
                        return;
                    }
                }
            }
        }
    }

    fn seek_finish(&mut self, found: Option<(MergeRecord, LineIdx)>) {
        let mut seek = self
            .pending_seek
            .take()
            .expect("seek_finish() called with seek in progress");
        let Some((found, line)) = found else {
            // When we don't find such a record, we don't change the anchor or
            // the rendered window.
            return;
        };

        // We found the record we're seeking to.  It's the last record seen by
        // the stepper.  If this was a backwards search, then the stepper is
        // pointed in the right spot already.  If it was a forwards search, then
        // it's just past the record we wanted.  Roll it back to point at that.
        // XXX-dap was this just wrong?
        // if seek.direction == Direction::Forward {
        //     // expect(): the stepper always keeps at least one previous record.
        //     seek.stepper.step_backward().expect("can step backwards");
        // };
        self.anchor_cursor = seek.stepper.cursor();
        self.anchor = Anchor::On {
            key: RecordKey {
                source_id: found.source_id().clone(),
                offset: found.offset(),
            },
            line: line.0,
        };
        // XXX-dap constants
        self.rendered =
            RenderedWindow::new(seek.stepper, 0, 1024, self.render_options);
    }
}

pub struct RenderedWindow {
    render_options: RenderOpts,
    records: Vec<WindowEntry>,
    state: PopulateState,
    materialized: Materialized,
    ordinals: HashMap<(SourceId, DateTime<Utc>), u64>,
    stepper: Stepper,
    nrecords: usize,
    forward_eof: bool,
}

#[derive(Debug)]
enum PopulateState {
    Backward(u32, u32),
    Forward(u32),
    Done,
}

impl PopulateState {
    fn initial(before: u32, after: u32) -> PopulateState {
        if before > 0 {
            PopulateState::Backward(before, after)
        } else if after > 0 {
            PopulateState::Forward(after)
        } else {
            PopulateState::Done
        }
    }

    fn next(&self, exhausted: bool) -> PopulateState {
        match *self {
            PopulateState::Done => PopulateState::Done,
            PopulateState::Backward(before, after) => {
                assert!(before > 0);
                if !exhausted && before > 1 {
                    PopulateState::Backward(before - 1, after)
                } else if after > 0 {
                    PopulateState::Forward(after)
                } else {
                    PopulateState::Done
                }
            }
            PopulateState::Forward(after) => {
                if !exhausted && after > 1 {
                    PopulateState::Forward(after - 1)
                } else {
                    PopulateState::Done
                }
            }
        }
    }
}

// XXX-dap currently unused
// struct WindowPosition {
//     record_idx: EventIdx,
//     line_idx: LineIdx,
// }

impl RenderedWindow {
    pub fn new(
        stepper: Stepper,
        before: u32,
        after: u32,
        render_options: RenderOpts,
    ) -> RenderedWindow {
        // unwrap(): we're not asking for that many records
        let nrecords = usize::try_from(after + before).unwrap();
        assert!(nrecords > 1); // XXX-dap
        let records = Vec::with_capacity(nrecords);
        let events = Vec::with_capacity(records.capacity());
        let formatted = Vec::new();
        let event_for_line = Vec::new();
        let first_line_for_event = Vec::with_capacity(records.capacity());
        let ordinals: HashMap<(SourceId, DateTime<Utc>), u64> = HashMap::new();
        let initial_state = PopulateState::initial(before, after);

        RenderedWindow {
            render_options,
            records,
            materialized: Materialized {
                events,
                formatted,
                event_for_line,
                first_line_for_event,
                parse_stats: Default::default(),
            },
            ordinals,
            stepper,
            state: initial_state,
            nrecords,
            forward_eof: false,
        }
    }

    pub fn is_populated(&self) -> bool {
        match self.state {
            PopulateState::Done => true,
            PopulateState::Backward(_, _) | PopulateState::Forward(_) => false,
        }
    }

    pub fn populate_work(&mut self) {
        // XXX-dap These steps might only do a tiny amount of work, or it might
        // exhaust our whole budget.  We can't really tell.
        let exhausted = match self.state {
            PopulateState::Done => true,
            PopulateState::Backward(remaining, _after) => {
                assert!(remaining > 0);
                let _event = self.stepper.step_backward();
                self.stepper.is_exhausted(Direction::Backward)
            }
            PopulateState::Forward(remaining) => {
                assert!(remaining > 0);
                if let Some(record) = self.stepper.step_forward() {
                    let entry = WindowEntry::new(record, &self.render_options);
                    self.records.push(entry);
                    self.render(self.records.len() - 1);
                    false
                } else {
                    self.stepper.is_exhausted(Direction::Forward)
                }
            }
        };

        if let PopulateState::Forward(..) = &self.state
            && exhausted
        {
            self.forward_eof = true;
        }

        self.state = self.state.next(exhausted);
    }

    fn render(&mut self, idx: usize) {
        let entry = &self.records[idx];
        let materialized = &mut self.materialized;
        let event_idx = EventIdx(materialized.events.len());
        materialized
            .first_line_for_event
            .push(LineIdx(materialized.formatted.len()));
        match entry.record.event() {
            Ok(event) => {
                let key = (entry.record.source_id().clone(), event.time);
                let ordinal = *self.ordinals.entry(key.clone()).or_insert(0);
                self.ordinals.insert(key, ordinal + 1);
                let position = LogStreamPosition::new(
                    entry.record.source_id().clone(),
                    event.time,
                    ordinal,
                );
                for line in &entry.lines {
                    materialized.formatted.push(line.clone());
                    materialized.event_for_line.push(event_idx);
                }
                materialized.events.push(Row::Event(EngineEvent {
                    position,
                    event: event.clone(),
                }));
            }
            Err(err) => {
                let msg = err.to_string();
                materialized.formatted.push(msg.clone());
                materialized.event_for_line.push(event_idx);
                materialized.events.push(Row::Error(msg));
            }
        }
    }

    pub fn set_render_options(&mut self, render_options: RenderOpts) {
        self.render_options = render_options;
        self.materialized = Materialized::new(self.records.len());
        self.ordinals.clear(); // XXX-dap belongs elsewhere?
        let mut records = Vec::with_capacity(self.records.len());
        std::mem::swap(&mut records, &mut self.records);
        for entry in records {
            self.records
                .push(WindowEntry::new(entry.record, &self.render_options));
        }
        for i in 0..self.records.len() {
            self.render(i);
        }
    }

    pub fn materialized(&self) -> &Materialized {
        &self.materialized
    }

    pub fn record_first(&self) -> Option<&MergeRecord> {
        self.records.first().map(|w| &w.record)
    }

    pub fn record_last(&self) -> Option<&MergeRecord> {
        self.records.last().map(|w| &w.record)
    }

    pub fn record_for_key(
        &self,
        record_key: &RecordKey,
    ) -> Option<&MergeRecord> {
        // XXX-dap this could be better
        self.records.iter().find_map(|entry| {
            (entry.key() == *record_key).then_some(&entry.record)
        })
    }

    pub fn stepper_at_cursor_within_window(&self, cursor: &Cursor) -> Stepper {
        // Choose a maximum step count just to avoid a logic bug resulting in an
        // infinite loop.
        let max_distance = self.nrecords + 1;

        // Start with our own stepper so that we can re-use as much of the data
        // as we can.
        let mut stepper = self.stepper.clone();

        // XXX-dap this is a problem!  This could take an unbounded amount of
        // time.  That's pretty unlikely though since the caller is always
        // trying to get to the anchor, and most windows will have been created
        // as a result of a seek to the anchor where they've already loaded that
        // record (and any others in between the cursor's current position and
        // the anchor).  This could be bad, though, if somebody navigated to a
        // bookmark with a selective filter that excluded the bookmarked record
        // and then immediately tried to seek relative to the anchor.  This
        // could cause the UI to hang up here while we try to find an unfiltered
        // record nearest to the anchor.
        let direction = match stepper.cursor().partial_cmp(cursor) {
            None => {
                panic!(
                    "stepper cursor is not ordered compared \
                     with requested cursor"
                );
            }
            Some(std::cmp::Ordering::Less) => Direction::Forward,
            Some(std::cmp::Ordering::Greater) => Direction::Backward,
            Some(std::cmp::Ordering::Equal) => {
                return stepper;
            }
        };

        for _ in 0..max_distance {
            let _ = stepper.step(direction);

            if *cursor == stepper.cursor() {
                return stepper;
            }
        }

        panic!(
            "did not find desired cursor within max_distance {max_distance}"
        );
    }

    fn is_forward_eof(&self) -> bool {
        self.forward_eof
    }

    // XXX-dap not used now, but maybe later
    // pub fn anchor_position(&self, anchor: &Anchor) -> Option<WindowPosition> {
    //     match anchor {
    //         Anchor::On { key, line } => {
    //             // XXX-dap this could be better
    //             self.records.iter().enumerate().find_map(|(i, entry)| {
    //                 (entry.key() == key).then_some((EventIdx(i), LineIdx(line)))
    //             })
    //         }
    //         Anchor::Empty | Anchor::PinFront => Some((EventIdx(0), LineIdx(0))),
    //         Anchor::PinBack => Some((
    //             EventIdx(self.records.len() - 1),
    //             LineIdx(self.records[last].lines.len() - 1),
    //         )),
    //     }
    // }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::test_fixtures::{TestDir, append_bunyan_at, append_raw, t};
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
        let mat = view.materialized();
        if mat.events.is_empty() {
            return None;
        }
        let line = view.anchor_flat_line();
        let event_idx = *mat.event_for_line.get(line)?;
        match &mat.events[event_idx.get()] {
            Row::Event(e) => Some(e.event.msg.clone()),
            Row::Error(_) => None,
        }
    }

    #[test]
    fn empty_engine_produces_empty_view() {
        let engine = Engine::new();
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        assert!(view.is_empty());
        assert!(view.materialized().events.is_empty());
        assert!(view.materialized().formatted.is_empty());
    }

    #[test]
    fn ensure_window_populates_from_default_cursor() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        assert_eq!(view.materialized().events.len(), 3);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m10"));
        let rendered: Vec<&str> =
            view.materialized().formatted.iter().map(|s| s.as_str()).collect();
        assert_eq!(rendered.len(), 3);
        assert!(rendered[0].contains("m10"));
        assert!(rendered[1].contains("m20"));
        assert!(rendered[2].contains("m30"));
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
        // fetches FETCH_BATCH_SIZE records (64), so any modest fixture
        // will fit; for this test we just verify scroll past the cached
        // set triggers more fetching.
        let dir = TestDir::new();
        let n = (FETCH_BATCH_SIZE * 2 + 5) as i64;
        let secs: Vec<i64> = (0..n).collect();
        let engine = build_engine(&[("a", &secs)], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 5);
        // Initial window is FETCH_BATCH_SIZE records.
        let initial = view.materialized().events.len();
        // Scroll past the initial window.
        for _ in 0..initial + 10 {
            view.scroll_lines(&engine, 1, 5);
        }
        // We should have fetched more.  Anchor is somewhere past the
        // initial cap.
        assert!(view.materialized().events.len() > initial || view.eof.forward);
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
    fn set_show_raw_renders_raw_bytes_from_source() {
        // Toggling `show_raw` should replace the formatted header with
        // the literal JSON bytes the line was read as.  Verify both the
        // toggle-on switch and the toggle-off return to the formatted
        // header.
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);

        let formatted = view.materialized().formatted[0].clone();
        assert!(
            formatted.contains("INFO") && formatted.contains("m10"),
            "expected formatted header, got {formatted:?}",
        );

        let mut o = view.render_opts();
        o.show_raw = true;
        view.set_render_opts(o);
        let raw_lines = view.materialized().formatted.clone();
        assert_eq!(raw_lines.len(), 1, "raw mode is one line per record");
        assert!(
            raw_lines[0].starts_with('{')
                && raw_lines[0].contains(r#""msg":"m10""#),
            "expected raw JSON line, got {:?}",
            raw_lines[0],
        );

        // Toggle off: header returns.
        o.show_raw = false;
        view.set_render_opts(o);
        let restored = view.materialized().formatted[0].clone();
        assert_eq!(restored, formatted);
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
        let dated_first = view.materialized().formatted[0].clone();
        assert!(
            dated_first.starts_with("1970-01-01T00:00:10.000Z "),
            "expected dated header, got {dated_first:?}",
        );

        let mut o = view.render_opts();
        o.show_date = false;
        view.set_render_opts(o);
        assert!(!view.render_opts().show_date);
        let undated_first = view.materialized().formatted[0].clone();
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
    fn bounded_records_to_scan_preserves_multi_source_order() {
        // Regression: the long-op driver bounds per-fill records-to-scan
        // so the UI stays responsive on selective filters.  With multi-source
        // merging, that means each source can hit its records-to-scan budget
        // mid-scan with no match yet — and popping a record from a
        // ready source before the others have walked to their next
        // match would emit records out of time order.  Verify that
        // the merge waits for every source to be ready (or at EOF)
        // before popping.
        let dir = TestDir::new();
        // Three sources with interleaved-in-time matches.  Source A
        // has a dense cluster early; source B has a single late
        // match; source C is dense early.  Without the all-ready
        // gate, the late B match could be popped before A and C have
        // walked far enough to see their earlier records, scrambling
        // the merge.
        let engine = build_engine(
            &[
                ("a", &[10, 20, 30, 40, 50]),
                ("b", &[1000]),
                ("c", &[15, 25, 35, 45]),
            ],
            &dir,
        );
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.prepare_seek_to_end(&engine).unwrap();
        // Drain the streamview's window via the long-op step
        // (bounded fills) until done — this exercises the
        // per-source walks-budget path that the synchronous
        // `seek_to_end` would skip.  Cap iterations to avoid an
        // infinite loop if the implementation ever regresses to
        // not-making-progress.
        let mut iters = 0;
        loop {
            match view.ensure_window_step(&engine, 30) {
                WindowFillStatus::Done => break,
                WindowFillStatus::NotDone => {}
            }
            iters += 1;
            if iters > 1000 {
                panic!("ensure_window_step did not converge");
            }
        }
        // Collect the materialized records' timestamps in deque
        // (oldest-first) order and assert they're monotonically
        // increasing.
        let times: Vec<i64> = view
            .materialized()
            .events
            .iter()
            .filter_map(|row| match row {
                Row::Event(e) => Some(e.event.time.timestamp()),
                Row::Error(_) => None,
            })
            .collect();
        let want = vec![10, 15, 20, 25, 30, 35, 40, 45, 50, 1000];
        assert_eq!(
            times, want,
            "multi-source backward fill must preserve time order",
        );
        dir.cleanup();
    }

    #[test]
    fn search_forward_extends_window_past_match() {
        // After a forward search Found, the cached window must have
        // at least viewport_height + OVER_FETCH_LINES of content at
        // or past the anchor.  Without that look-ahead, the TUI's
        // `resync_from_streamview` clamps `viewport_top` to `max_top`
        // and subsequent `j` keystrokes don't visibly advance the
        // view — the user-visible symptom of the
        // navigation-stops-after-search bug.
        //
        // Use a 300-record file (well past the initial 138-line pop)
        // and search to the record at index 130 of the initial pop —
        // which is 8 records past the back of the initial pop, so
        // without the extend-after-Found logic there would be 0
        // records past the anchor.
        let dir = TestDir::new();
        let secs: Vec<i64> = (10..310).collect();
        let engine = build_engine(&[("a", &secs)], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 10);
        // `m140 ` (trailing space) so the regex matches only the
        // exact message — `m140` alone would also match `m1400`,
        // `m1401`, … if the test fixture extended further; the
        // trailing space pins it to the rendered "<header>  m140" line.
        let regex = Regex::new(r"m140\b").unwrap();
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            SearchAnchor::Include,
            10,
            SEARCH_BUDGET,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::Found);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m140"));

        let anchor_flat = view.anchor_flat_line();
        let total_flat: usize = view.materialized().formatted.len();
        let lines_at_or_past_anchor = total_flat - anchor_flat;
        let want = 10 + OVER_FETCH_LINES;
        assert!(
            lines_at_or_past_anchor >= want,
            "expected >= {want} lines at/past anchor, got \
             {lines_at_or_past_anchor} (anchor_flat={anchor_flat}, \
             total_flat={total_flat})",
        );
        dir.cleanup();
    }

    #[test]
    fn search_step_finds_match_in_window() {
        let dir = TestDir::new();
        let engine = build_engine(&[("a", &[10, 20, 30])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let regex = Regex::new("m20").unwrap();
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            SearchAnchor::Include,
            20,
            SEARCH_BUDGET,
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
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            SearchAnchor::Include,
            20,
            SEARCH_BUDGET,
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
        let _ = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            SearchAnchor::Include,
            20,
            SEARCH_BUDGET,
            &mut never_cancel(),
        );
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        let first_line = view.anchor_flat_line();
        // Next match (exclusive=true): the second m20 at idx 3.
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            SearchAnchor::Skip,
            20,
            SEARCH_BUDGET,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::Found);
        // Both records have msg "m20"; verify the anchor advanced
        // rather than re-landing on the first match.
        assert_eq!(anchor_msg(&view).as_deref(), Some("m20"));
        assert!(view.anchor_flat_line() > first_line);
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
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Backward,
            SearchAnchor::Skip,
            20,
            SEARCH_BUDGET,
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
        let engine = build_engine(&[("a", &[10, 20, 30, 40, 50])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let regex = Regex::new("m50").unwrap();
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            SearchAnchor::Include,
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
            SearchAnchor::Include,
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
        let engine = build_engine(&[("a", &[10, 20, 30, 40, 50])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let regex = Regex::new("nonexistent").unwrap();
        // Exhaust the budget without finding anything.
        let outcome = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            SearchAnchor::Include,
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
            SearchAnchor::Include,
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
        let engine = build_engine(&[("a", &[10, 20, 30, 40, 50])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let r1 = Regex::new("nonexistent").unwrap();
        let outcome = view.search_step_with_budget(
            &engine,
            &r1,
            SearchDir::Forward,
            SearchAnchor::Include,
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
            SearchAnchor::Include,
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
        let engine = build_engine(&[("a", &[10, 20, 30, 40, 50])], &dir);
        let mut view =
            StreamView::new(Filter::default(), RenderOpts::default());
        view.ensure_window(&engine, 20);
        let regex = Regex::new("m30").unwrap();
        // Find m30 (record 2) within a generous budget.
        let _ = view.search_step_with_budget(
            &engine,
            &regex,
            SearchDir::Forward,
            SearchAnchor::Include,
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
            SearchAnchor::Skip,
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
            SearchAnchor::Include,
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
            SearchAnchor::Include,
            20,
            10,
            &mut never_cancel(),
        );
        assert_eq!(outcome, SearchOutcome::Found);
        assert_eq!(anchor_msg(&view).as_deref(), Some("m30"));
        dir.cleanup();
    }

    #[test]
    fn parse_errors_appear_inline_in_materialized_output() {
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
        assert_eq!(view.materialized().events.len(), 3);
        let rendered: Vec<&str> =
            view.materialized().formatted.iter().map(|s| s.as_str()).collect();
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
        let mut v0 = StreamView::new(Filter::default(), RenderOpts::default());
        v0.seek_to_cursor(&engine, c0, 20);
        assert_eq!(anchor_msg(&v0).as_deref(), Some("m10"));
        // A middle record: walks past two preceding entries.
        let c2 = view.cursor_before_record(2).unwrap();
        let mut v2 = StreamView::new(Filter::default(), RenderOpts::default());
        v2.seek_to_cursor(&engine, c2, 20);
        assert_eq!(anchor_msg(&v2).as_deref(), Some("m30"));
        // Past the end is a clean None.
        assert!(
            view.cursor_before_record(view.materialized().events.len())
                .is_none()
        );
        dir.cleanup();
    }

    #[test]
    fn cursor_before_record_handles_multiple_sources() {
        // With two interleaved sources, the cursor for a record from
        // source B must capture A's "just past last seen" offset too —
        // otherwise a forward step from the cursor would re-emit A's
        // earlier records.
        let dir = TestDir::new();
        let engine =
            build_engine(&[("a", &[10, 30, 50]), ("b", &[20, 40, 60])], &dir);
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
}
