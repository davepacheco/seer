// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Merge stepper: forward/backward k-way merge over the engine's sources
//! with per-source lookahead and lookbehind buffers.
//!
//! [`Stepper`] is the navigation engine that powers the TUI's scrolling.
//! Unlike [`super::EventStream`], which materializes every event in one
//! full pass, the stepper fetches lazily via [`Source::query`] and caches
//! a bounded window of records in each direction so small navigation
//! moves don't trigger fresh I/O.
//!
//! Use [`super::Engine::stepper`] to construct one.  The stepper is
//! restored to a previous [`Cursor`] on construction; calling
//! [`Stepper::cursor`] at any point produces a serializable snapshot of
//! the current position.

use crate::event::Event;
use crate::filter::Filter;
use crate::position::{ByteLen, ByteOffset, Cursor, SourceId};
use crate::source::{Direction, QueryRecord, Source, SourceError};
use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::sync::Arc;
use thiserror::Error;

/// Returns the opposite direction.
fn opposite(d: Direction) -> Direction {
    match d {
        Direction::Forward => Direction::Backward,
        Direction::Backward => Direction::Forward,
    }
}

/// Default per-fetch batch size for the merge stepper.  Each refill
/// of a [`SourceWindow`] asks the storage layer for up to this many
/// matching records; [`crate::streamview::StreamView`] uses the same
/// value when it asks the stepper to extend a cached window in either
/// direction, so the two layers stay in lock-step instead of one
/// over-fetching beyond what the other can hold.  Larger batches
/// amortize seek cost; smaller batches keep the maximum work per step
/// bounded when the active filter rejects most records (the storage
/// layer walks until `count` matches are found, so the wall time per
/// `query` call scales with this constant under selective filters).
/// The long-op driver behind `G`/`g`/filter rebuild overrides this
/// with a small value via [`super::Engine::stepper_with_batch`] so each
/// tick yields after a small chunk of walking and the UI stays
/// responsive.
pub const FETCH_BATCH_SIZE: usize = 64;

/// Maximum records held in either direction's buffer for a single
/// source.  When a step would push beyond this, the oldest entry on
/// the *opposite* end is dropped and that direction's EOF flag is
/// cleared so a subsequent fetch can re-acquire the dropped data.
const BUFFER_LIMIT: usize = 256;

/// A single record produced by [`Stepper::step_forward`] /
/// [`Stepper::step_backward`].
///
/// Holds either a parsed [`Event`] or a [`MergeError`] for a per-line
/// error encountered while reading.  A synthetic [`MergeRecord`] with
/// `length == 0` is emitted when the storage layer's `query` itself
/// fails (e.g. the file was deleted out from under us); a real
/// on-disk record always has positive length.
#[derive(Debug, Clone)]
pub struct MergeRecord {
    /// source the record came from
    pub source_id: SourceId,
    /// byte offset in `source_id` where the record begins
    pub offset: ByteOffset,
    /// length of the record in bytes (including the trailing newline,
    /// if present); [`ByteLen::ZERO`] for synthetic error placeholders
    pub length: ByteLen,
    /// parsed event when the line was valid; otherwise the per-line
    /// parse or I/O error
    pub event: Result<Event, MergeError>,
    /// the record's bytes as they appear in the source, minus any
    /// trailing line terminator; empty for synthetic error placeholders
    pub raw: String,
}

/// Per-line error surfaced by [`Stepper`].
///
/// Wraps a shared [`SourceError`] so the merge cursor's buffers can
/// hand out `Clone` copies cheaply (the underlying `std::io::Error` /
/// `serde_json::Error` are not `Clone`).  The original variant is
/// preserved so consumers can match on parse vs. I/O if useful;
/// most callers just render via [`Display`].
#[derive(Debug, Clone, Error)]
#[error(transparent)]
pub struct MergeError(#[from] Arc<SourceError>);

/// Whether a directional scan has exhausted the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EofMark {
    /// The scan in this direction has hit EOF; further fetches in
    /// this direction are short-circuited until the flag is cleared.
    Reached,
    /// The scan has not (or no longer has) hit EOF; future fetches
    /// will hit the storage layer.
    Cleared,
}

/// Internal cloneable buffer entry — same fields as [`MergeRecord`] but
/// without the per-record `source_id` (it's a property of the owning
/// [`SourceWindow`]).
#[derive(Debug, Clone)]
struct BufferedRecord {
    offset: ByteOffset,
    length: ByteLen,
    event: Result<Event, MergeError>,
    raw: String,
}

impl From<QueryRecord> for BufferedRecord {
    fn from(record: QueryRecord) -> Self {
        let QueryRecord { offset, length, event, raw } = record;
        Self {
            offset,
            length,
            event: event.map_err(|e| MergeError::from(Arc::new(e))),
            raw,
        }
    }
}

/// Per-source state for the merge stepper: a byte-offset cursor, paired
/// lookahead/lookbehind buffers, and per-direction EOF flags.
struct SourceWindow {
    source: Arc<dyn Source>,
    /// Byte-offset cursor.  Forward step would read the record starting
    /// at `position`; backward step would read the record ending at
    /// `position`.
    position: ByteOffset,
    /// Records starting at or after `position`, earliest-first.  When
    /// non-empty, `forward_buf.front().offset == position`.
    forward_buf: VecDeque<BufferedRecord>,
    /// Records ending at or before `position`, most-recent-first.  When
    /// non-empty,
    /// `backward_buf.front().offset + backward_buf.front().length ==
    /// position`.
    backward_buf: VecDeque<BufferedRecord>,
    /// Source has no more records past the cached forward window.
    /// Cleared whenever cached forward records are dropped.
    forward_eof: bool,
    /// Symmetric.
    backward_eof: bool,
}

impl SourceWindow {
    fn new(source: Arc<dyn Source>, position: ByteOffset) -> Self {
        Self {
            source,
            position,
            forward_buf: VecDeque::new(),
            backward_buf: VecDeque::new(),
            forward_eof: false,
            backward_eof: false,
        }
    }

    fn buf(&self, dir: Direction) -> &VecDeque<BufferedRecord> {
        match dir {
            Direction::Forward => &self.forward_buf,
            Direction::Backward => &self.backward_buf,
        }
    }

    fn buf_mut(&mut self, dir: Direction) -> &mut VecDeque<BufferedRecord> {
        match dir {
            Direction::Forward => &mut self.forward_buf,
            Direction::Backward => &mut self.backward_buf,
        }
    }

    fn eof(&self, dir: Direction) -> bool {
        match dir {
            Direction::Forward => self.forward_eof,
            Direction::Backward => self.backward_eof,
        }
    }

    fn set_eof(&mut self, dir: Direction, mark: EofMark) {
        let value = matches!(mark, EofMark::Reached);
        match dir {
            Direction::Forward => self.forward_eof = value,
            Direction::Backward => self.backward_eof = value,
        }
    }

    /// Ensures the head of `dir`'s buffer is populated, fetching from
    /// the storage layer when needed.  No-op when the buffer is
    /// non-empty or the direction is already known to be exhausted.
    /// Returns the number of bytes the query walked off disk —
    /// including bytes from records the filter rejected.
    fn fill(
        &mut self,
        dir: Direction,
        filter: &Filter,
        batch_size: usize,
        max_records_to_scan: Option<usize>,
    ) -> ByteLen {
        if !self.buf(dir).is_empty() || self.eof(dir) {
            return ByteLen::ZERO;
        }
        // A backward query at offset 0 has no records to return — short
        // circuit so we don't even open the file.
        if matches!(dir, Direction::Backward) && self.position.get() == 0 {
            self.set_eof(dir, EofMark::Reached);
            return ByteLen::ZERO;
        }
        match self.source.query_bounded(
            self.position,
            dir,
            batch_size,
            max_records_to_scan,
            filter,
        ) {
            Ok(batch) => {
                if batch.eof {
                    self.set_eof(dir, EofMark::Reached);
                }
                let walked = batch.walked_bytes;
                // When the query walked records without finding any
                // match — whether because the budget expired in a
                // sparse filter region or because the walk ran clear
                // through to EOF without a hit — advance our position
                // to the scan's end.  The budget case is the "don't
                // loop forever re-scanning the same prefix" case; the
                // EOF case is needed so `stepper.cursor()` reflects
                // how far we've walked in a fully-filtered source
                // (otherwise the StreamView's user-status byte offset
                // under-reports — see `front_cursor` use in
                // `cursor_before_record`).  When matches *were*
                // found, leave position alone: `pop` will set it to
                // the most-recently-popped record's offset, which is
                // the right cursor for both `stepper.cursor()` and
                // the next refill.  (Some records in the scanned
                // region past the last match may be re-walked on the
                // next refill; that redundancy is bounded by
                // `batch_size` and is the cost of keeping `pop`'s
                // position semantics intact.)
                if batch.records.is_empty() {
                    match dir {
                        Direction::Forward => self.position += walked,
                        Direction::Backward => {
                            self.position = ByteOffset::from(
                                self.position
                                    .get()
                                    .saturating_sub(walked.get()),
                            );
                        }
                    }
                }
                let buf = self.buf_mut(dir);
                for r in batch.records {
                    buf.push_back(BufferedRecord::from(r));
                }
                walked
            }
            Err(e) => {
                // Surface the I/O failure inline as a synthetic
                // zero-length record at the current frontier so it
                // shows up in the merged stream the same as a per-line
                // parse error, then mark the direction exhausted to
                // avoid hammering the failed fetch.
                let synth = BufferedRecord {
                    offset: self.position,
                    length: ByteLen::ZERO,
                    event: Err(MergeError::from(Arc::new(SourceError::from(
                        e,
                    )))),
                    raw: String::new(),
                };
                self.buf_mut(dir).push_back(synth);
                self.set_eof(dir, EofMark::Reached);
                ByteLen::ZERO
            }
        }
    }

    /// Pops `dir`'s head and emits it, mirroring the record into the
    /// opposite buffer so a subsequent step in the opposite direction
    /// replays it without I/O.  Caller must ensure the head exists.
    fn pop(&mut self, dir: Direction) -> MergeRecord {
        let r = self.buf_mut(dir).pop_front().expect("buf has a head");
        self.position = match dir {
            Direction::Forward => r.offset + r.length,
            Direction::Backward => r.offset,
        };
        let opp = opposite(dir);
        self.buf_mut(opp).push_front(r.clone());
        // Stepping in `dir` exposes new ground in the opposite
        // direction, so any prior "exhausted" determination there is
        // stale.  Trimming below would clear it again; we set it once
        // up front and let the trim be a pure size operation.
        self.set_eof(opp, EofMark::Cleared);
        while self.buf(opp).len() > BUFFER_LIMIT {
            self.buf_mut(opp).pop_back();
        }
        MergeRecord {
            source_id: self.source.id().clone(),
            offset: r.offset,
            length: r.length,
            event: r.event,
            raw: r.raw,
        }
    }

    /// Drops both buffers and clears EOF flags.  Used on filter changes
    /// — the buffered records were filtered against the previous
    /// filter so they can no longer be trusted.
    fn clear_buffers(&mut self) {
        self.forward_buf.clear();
        self.backward_buf.clear();
        self.forward_eof = false;
        self.backward_eof = false;
    }
}

/// Optional knobs for [`Stepper`] construction.
///
/// The default value (used by [`super::Engine::stepper`] and
/// [`Stepper::new`]) matches every common navigation path: the storage
/// layer's [`FETCH_BATCH_SIZE`] batch, no per-fill records-to-scan budget.
/// The TUI's long-op driver passes a customized value through
/// [`super::Engine::stepper_with`] to bound the wall time per
/// `stepper.step` call under selective filters.
#[derive(Debug, Clone)]
pub struct StepperOptions {
    /// Per-fill batch size handed to the storage layer's `query`.
    /// Defaults to [`FETCH_BATCH_SIZE`].
    pub batch_size: usize,
    /// When `Some(n)`, each per-source fill examines at most `n`
    /// records (matching *or* filter-rejected) before returning.  The
    /// long-op driver passes a small value here so each `step` call
    /// yields after a bounded amount of walking even when the filter
    /// rejects almost everything.
    pub max_records_to_scan_per_fill: Option<usize>,
}

impl Default for StepperOptions {
    fn default() -> Self {
        Self {
            batch_size: FETCH_BATCH_SIZE,
            max_records_to_scan_per_fill: None,
        }
    }
}

/// Forward/backward k-way merge over the engine's sources with per-source
/// lookahead and lookbehind buffers.
///
/// Acquired from [`super::Engine::stepper`].  Each call to
/// [`Self::step_forward`] / [`Self::step_backward`] returns the next
/// record in time order across the sources, fetching from the
/// underlying [`Source`] only as needed.  The set of sources is fixed at
/// construction; filter changes are applied via [`Self::set_filter`]
/// (which drops buffered records but retains per-source byte offsets).
pub struct Stepper {
    sources: Vec<SourceWindow>,
    filter: Filter,
    batch_size: usize,
    /// When `Some(n)`, each [`SourceWindow::fill`] examines at most
    /// `n` records (matching *or* filtered) before returning.  Lets
    /// the long-op driver bound the wall time per fill on selective
    /// filters where a single batch of matches would otherwise force
    /// the underlying scan to walk thousands of on-disk records.
    max_records_to_scan_per_fill: Option<usize>,
    /// Running total of bytes the stepper's fills have walked off
    /// disk — including bytes from records the filter rejected.
    /// Surfaced to callers via [`Self::walked_bytes`] so the TUI's
    /// progress bar can tick even when fills produce no matches.
    walked_bytes: ByteLen,
}

impl Stepper {
    /// Internal constructor with default [`StepperOptions`].  Used by
    /// this module's own tests; public callers go through
    /// [`super::Engine::stepper`].
    #[cfg(test)]
    fn new(
        sources: Vec<Arc<dyn Source>>,
        filter: Filter,
        cursor: &Cursor,
    ) -> Self {
        Self::with_options(sources, filter, cursor, StepperOptions::default())
    }

    /// Internal constructor that lets the caller customize the
    /// per-fill batch size and records-to-scan budget.  Public callers go
    /// through [`super::Engine::stepper_with`].
    pub(super) fn with_options(
        sources: Vec<Arc<dyn Source>>,
        filter: Filter,
        cursor: &Cursor,
        options: StepperOptions,
    ) -> Self {
        let StepperOptions { batch_size, max_records_to_scan_per_fill } =
            options;
        let windows = sources
            .into_iter()
            .map(|s| {
                let pos = cursor.get(s.id()).unwrap_or(ByteOffset::ZERO);
                SourceWindow::new(s, pos)
            })
            .collect();
        Self {
            sources: windows,
            filter,
            batch_size,
            max_records_to_scan_per_fill,
            walked_bytes: ByteLen::ZERO,
        }
    }

    /// Total bytes the stepper has walked off disk since construction
    /// (matching plus filter-rejected records).
    pub fn walked_bytes(&self) -> ByteLen {
        self.walked_bytes
    }

    /// True iff every per-source window has hit EOF in `dir`.  Used
    /// by callers running the stepper under a per-fill records-to-scan budget
    /// to distinguish a budget-exhausted `step` (returns `None` but
    /// has more records to find on a subsequent call) from real
    /// source exhaustion.
    pub fn is_exhausted(&self, dir: Direction) -> bool {
        self.sources.iter().all(|s| s.eof(dir))
    }

    /// Returns the active filter.
    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// Replaces the active filter.  Drops every buffered record (since
    /// they were filtered against the previous filter) and clears the
    /// per-source EOF flags so a fresh fetch sees what the new filter
    /// accepts.  Per-source byte offsets are retained — the cursor's
    /// position does not move.
    pub fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
        for s in &mut self.sources {
            s.clear_buffers();
        }
    }

    /// Returns a snapshot of every source's current byte offset,
    /// suitable for serialization.
    pub fn cursor(&self) -> Cursor {
        Cursor::with(
            self.sources.iter().map(|s| (s.source.id().clone(), s.position)),
        )
    }

    /// Returns the next record in time order, or `None` when every
    /// source is forward-exhausted.
    pub fn step_forward(&mut self) -> Option<MergeRecord> {
        self.step(Direction::Forward)
    }

    /// Returns the previous record in reverse time order, or `None`
    /// when every source is backward-exhausted.
    pub fn step_backward(&mut self) -> Option<MergeRecord> {
        self.step(Direction::Backward)
    }

    fn step(&mut self, dir: Direction) -> Option<MergeRecord> {
        // Multi-source merge requires every source to be "ready" —
        // either holding a buffered head or at EOF in this direction
        // — before we can safely pick one to pop.  Without that, a
        // selective filter under a bounded records-to-scan budget can
        // leave one source mid-scan while another has surfaced its
        // next match; popping that match would emit out of time order
        // (the still-scanning source could turn up a record with a
        // closer time on a later fill).
        //
        // The loop refills non-ready sources until every one of them
        // has a head or has hit its direction's edge, or until no
        // source progressed in an iteration (only possible under a
        // records-to-scan budget that expired without surfacing a
        // match).  `progressed` tracks whether a fill actually made a
        // previously non-ready source ready (surfaced a record or hit
        // EOF), not merely that fill was called.  When the loop exits
        // with some sources still mid-scan, the post-loop check
        // refuses to pick and returns `None`; the caller can drive
        // another step to resume the scan, and distinguishes this
        // budget-suspended `None` from true exhaustion via
        // [`Self::is_exhausted`].
        loop {
            let mut progressed = false;
            for s in &mut self.sources {
                if !s.buf(dir).is_empty() || s.eof(dir) {
                    continue;
                }
                let walked = s.fill(
                    dir,
                    &self.filter,
                    self.batch_size,
                    self.max_records_to_scan_per_fill,
                );
                self.walked_bytes += walked;
                if !s.buf(dir).is_empty() || s.eof(dir) {
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        if !self.sources.iter().all(|s| !s.buf(dir).is_empty() || s.eof(dir)) {
            return None;
        }
        let idx = pick(&self.sources, dir)?;
        Some(self.sources[idx].pop(dir))
    }
}

/// Picks the source whose head should be emitted next.
///
/// For [`Direction::Forward`]: the head is `forward_buf.front()`;
/// among non-error heads the smallest timestamp wins; ties break by
/// lowest source-add index.  An error head wins over any event head,
/// regardless of time.
///
/// For [`Direction::Backward`]: the head is `backward_buf.front()`;
/// among non-error heads the largest timestamp wins; ties break by
/// *highest* source-add index — the reverse of the forward tiebreaker
/// — so backward stepping over a run of equal-timestamped events from
/// multiple sources retraces the forward emit order in reverse.  Among
/// multiple error heads the same direction-aware tiebreak applies.
fn pick(sources: &[SourceWindow], direction: Direction) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_is_err = false;
    let mut best_time: Option<DateTime<Utc>> = None;
    for (i, s) in sources.iter().enumerate() {
        let Some(head) = s.buf(direction).front() else { continue };
        match &head.event {
            Err(_) => {
                // Errors emit eagerly: an error head always wins over
                // an event head.  Forward keeps the first error
                // encountered (lowest index); backward overwrites so
                // the highest index wins.
                match (best_is_err, direction) {
                    (false, _) => {
                        best = Some(i);
                        best_is_err = true;
                    }
                    (true, Direction::Backward) => {
                        best = Some(i);
                    }
                    (true, Direction::Forward) => {}
                }
            }
            Ok(event) => {
                if best_is_err {
                    continue;
                }
                let t = event.time;
                // Strict inequality on time, then ties go to higher
                // index for backward (lowest-index wins forward by
                // being set first and never replaced).
                let take = match direction {
                    Direction::Forward => best_time.is_none_or(|bt| t < bt),
                    Direction::Backward => match best_time {
                        None => true,
                        Some(bt) => t >= bt,
                    },
                };
                if take {
                    best = Some(i);
                    best_time = Some(t);
                }
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FileSource;
    use crate::test_fixtures::{TestDir, append_bunyan_at, append_raw, t};
    use camino::Utf8Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Writes `secs.len()` bunyan records to `path` with consecutive
    /// timestamps and messages `m{sec}`.
    fn write_fixture(path: &Utf8Path, name: &str, secs: &[i64]) {
        for s in secs {
            append_bunyan_at(path, name, t(*s), &format!("m{s}"));
        }
    }

    /// Builds a [`Stepper`] over the given sources, all starting at
    /// byte zero with the default filter.  Clones the `Arc`s into the
    /// stepper, so the caller's slice can be reused for `id()`
    /// lookups while the stepper runs.
    fn make_stepper(sources: &[Arc<dyn Source>]) -> Stepper {
        Stepper::new(sources.to_vec(), Filter::default(), &Cursor::new())
    }

    /// Drains a stepper forward and returns just the parsed event
    /// messages, panicking on any error item.  Used by tests that
    /// build error-free fixtures.
    fn forward_msgs(stepper: &mut Stepper) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(r) = stepper.step_forward() {
            match r.event {
                Ok(e) => out.push(e.msg),
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        out
    }

    /// Symmetric: drains a stepper backward.
    fn backward_msgs(stepper: &mut Stepper) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(r) = stepper.step_backward() {
            match r.event {
                Ok(e) => out.push(e.msg),
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        out
    }

    #[test]
    fn empty_engine_yields_no_steps() {
        // No sources at all: both directions return None immediately.
        let sources: Vec<Arc<dyn Source>> = Vec::new();
        let mut stepper = make_stepper(&sources);
        assert!(stepper.step_forward().is_none());
        assert!(stepper.step_backward().is_none());
        assert!(stepper.cursor().is_empty());
    }

    #[test]
    fn empty_source_yields_no_steps() {
        let dir = TestDir::new();
        let p = dir.path().join("empty.log");
        std::fs::File::create(&p).unwrap();
        let src: Arc<dyn Source> = Arc::new(FileSource::open(&p).unwrap());
        let sources = vec![src];
        let mut stepper = make_stepper(&sources);
        assert!(stepper.step_forward().is_none());
        assert!(stepper.step_backward().is_none());
        // Cursor still records the source — at byte zero.
        assert_eq!(stepper.cursor().len(), 1);
        dir.cleanup();
    }

    #[test]
    fn single_source_forward_yields_records_in_order() {
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        write_fixture(&p, "x", &[10, 20, 30]);
        let src: Arc<dyn Source> = Arc::new(FileSource::open(&p).unwrap());
        let sources = vec![src];
        let mut stepper = make_stepper(&sources);
        assert_eq!(forward_msgs(&mut stepper), vec!["m10", "m20", "m30"]);
        // After draining forward, another forward step is None.
        assert!(stepper.step_forward().is_none());
        dir.cleanup();
    }

    #[test]
    fn single_source_backward_from_default_cursor_is_empty() {
        // A fresh stepper sits at byte 0 of every source; backward
        // from byte 0 has nothing to return.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        write_fixture(&p, "x", &[10, 20]);
        let src: Arc<dyn Source> = Arc::new(FileSource::open(&p).unwrap());
        let sources = vec![src];
        let mut stepper = make_stepper(&sources);
        assert!(stepper.step_backward().is_none());
        dir.cleanup();
    }

    #[test]
    fn forward_then_backward_round_trips() {
        // Stepping forward N times then backward N times yields the
        // forward sequence reversed.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        write_fixture(&p, "x", &[10, 20, 30, 40]);
        let src: Arc<dyn Source> = Arc::new(FileSource::open(&p).unwrap());
        let sources = vec![src];
        let mut stepper = make_stepper(&sources);
        let mut fwd = Vec::new();
        for _ in 0..4 {
            let r = stepper.step_forward().unwrap();
            fwd.push(r.event.unwrap().msg);
        }
        let mut bwd = Vec::new();
        for _ in 0..4 {
            let r = stepper.step_backward().unwrap();
            bwd.push(r.event.unwrap().msg);
        }
        let mut fwd_rev = fwd.clone();
        fwd_rev.reverse();
        assert_eq!(bwd, fwd_rev);
        // Now stepping backward more is None (we're at byte 0).
        assert!(stepper.step_backward().is_none());
        // And stepping forward replays the original sequence.
        let replay = forward_msgs(&mut stepper);
        assert_eq!(replay, fwd);
        dir.cleanup();
    }

    #[test]
    fn multi_source_forward_merges_by_time() {
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        write_fixture(&a, "x", &[10, 30]);
        write_fixture(&b, "x", &[20, 40]);
        let sa: Arc<dyn Source> = Arc::new(FileSource::open(&a).unwrap());
        let sb: Arc<dyn Source> = Arc::new(FileSource::open(&b).unwrap());
        let sources = vec![sa, sb];
        let mut stepper = make_stepper(&sources);
        assert_eq!(
            forward_msgs(&mut stepper),
            vec!["m10", "m20", "m30", "m40"],
        );
        dir.cleanup();
    }

    #[test]
    fn multi_source_backward_is_reverse_of_forward() {
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        write_fixture(&a, "x", &[10, 30]);
        write_fixture(&b, "x", &[20, 40]);
        let sa: Arc<dyn Source> = Arc::new(FileSource::open(&a).unwrap());
        let sb: Arc<dyn Source> = Arc::new(FileSource::open(&b).unwrap());
        let sources = vec![sa, sb];
        let mut stepper = make_stepper(&sources);
        // Drive forward to the end first.
        let fwd = forward_msgs(&mut stepper);
        let bwd = backward_msgs(&mut stepper);
        let mut expected = fwd.clone();
        expected.reverse();
        assert_eq!(bwd, expected);
        dir.cleanup();
    }

    #[test]
    fn forward_breaks_ties_by_source_add_order() {
        // Both events at the same instant; source A added first, so its
        // event emits first.
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        append_bunyan_at(&a, "x", t(50), "a-tie");
        append_bunyan_at(&b, "x", t(50), "b-tie");
        let sa: Arc<dyn Source> = Arc::new(FileSource::open(&a).unwrap());
        let sb: Arc<dyn Source> = Arc::new(FileSource::open(&b).unwrap());
        let sources = vec![sa, sb];
        let mut stepper = make_stepper(&sources);
        assert_eq!(forward_msgs(&mut stepper), vec!["a-tie", "b-tie"]);
        dir.cleanup();
    }

    #[test]
    fn backward_ties_emit_in_reverse_of_forward_ties() {
        // Three sources, all with one event at the same instant.
        // Forward order is 0,1,2 (lowest index first); backward order
        // must be 2,1,0 so that forward-then-backward symmetry holds
        // even at exact-tie groups.
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        let c = dir.path().join("c.log");
        append_bunyan_at(&a, "x", t(50), "tie-a");
        append_bunyan_at(&b, "x", t(50), "tie-b");
        append_bunyan_at(&c, "x", t(50), "tie-c");
        let sa: Arc<dyn Source> = Arc::new(FileSource::open(&a).unwrap());
        let sb: Arc<dyn Source> = Arc::new(FileSource::open(&b).unwrap());
        let sc: Arc<dyn Source> = Arc::new(FileSource::open(&c).unwrap());
        let sources = vec![sa, sb, sc];
        let mut stepper = make_stepper(&sources);
        let fwd = forward_msgs(&mut stepper);
        assert_eq!(fwd, vec!["tie-a", "tie-b", "tie-c"]);
        let bwd = backward_msgs(&mut stepper);
        assert_eq!(bwd, vec!["tie-c", "tie-b", "tie-a"]);
        dir.cleanup();
    }

    #[test]
    fn cursor_round_trips_across_steppers() {
        // Step a few times, snapshot the cursor, build a fresh stepper
        // from that cursor, and verify the remaining records match.
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        write_fixture(&a, "x", &[10, 30, 50]);
        write_fixture(&b, "x", &[20, 40, 60]);
        let sa: Arc<dyn Source> = Arc::new(FileSource::open(&a).unwrap());
        let sb: Arc<dyn Source> = Arc::new(FileSource::open(&b).unwrap());
        let sources = vec![sa, sb];
        let mut stepper = make_stepper(&sources);
        // Consume the first three events: m10, m20, m30.
        for _ in 0..3 {
            stepper.step_forward().unwrap();
        }
        let snapshot = stepper.cursor();
        // Build a brand-new stepper at that cursor.
        let mut resumed =
            Stepper::new(sources.clone(), Filter::default(), &snapshot);
        assert_eq!(forward_msgs(&mut resumed), vec!["m40", "m50", "m60"],);
        dir.cleanup();
    }

    #[test]
    fn cursor_default_starts_at_byte_zero() {
        // A `Cursor::default()` carries no entries, so every source
        // resolves to byte zero — the stepper walks from the start.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        write_fixture(&p, "x", &[10, 20]);
        let src: Arc<dyn Source> = Arc::new(FileSource::open(&p).unwrap());
        let sources = vec![src];
        let mut stepper =
            Stepper::new(sources, Filter::default(), &Cursor::default());
        assert_eq!(forward_msgs(&mut stepper), vec!["m10", "m20"]);
        dir.cleanup();
    }

    #[test]
    fn cursor_at_eof_walks_backward_only() {
        // Build a cursor pointing at end-of-file; forward step yields
        // nothing, backward yields the records in reverse.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        write_fixture(&p, "x", &[10, 20, 30]);
        let len = std::fs::metadata(&p).unwrap().len();
        let src: Arc<dyn Source> = Arc::new(FileSource::open(&p).unwrap());
        let id = src.id().clone();
        let sources = vec![src];
        let cursor = Cursor::with([(id, ByteOffset::from(len))]);
        let mut stepper = Stepper::new(sources, Filter::default(), &cursor);
        assert!(stepper.step_forward().is_none());
        assert_eq!(backward_msgs(&mut stepper), vec!["m30", "m20", "m10"],);
        dir.cleanup();
    }

    #[test]
    fn cursor_serializes_as_bare_map() {
        // The `#[serde(transparent)]` annotation means a Cursor
        // round-trips through serde as the inner BTreeMap directly,
        // so persisted bookmarks don't acquire a wrapping struct
        // tag that would later need migration.
        let id = SourceId::from("s1".to_string());
        let cursor = Cursor::with([(id, ByteOffset::from(100))]);
        let json = serde_json::to_string(&cursor).unwrap();
        assert_eq!(json, r#"{"s1":100}"#);
        let back: Cursor = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cursor);
    }

    #[test]
    fn set_filter_drops_buffers_but_keeps_position() {
        // Walk forward a few records, change the filter to one that
        // drops the next record, and confirm that the next forward
        // step honors the new filter without re-emitting earlier ones.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        for s in &[10i64, 20, 30, 40] {
            append_bunyan_at(&p, "x", t(*s), &format!("m{s}"));
        }
        let src: Arc<dyn Source> = Arc::new(FileSource::open(&p).unwrap());
        let sources = vec![src];
        let mut stepper = make_stepper(&sources);
        // Consume m10 and m20.
        let r1 = stepper.step_forward().unwrap();
        assert_eq!(r1.event.as_ref().unwrap().msg, "m10");
        let r2 = stepper.step_forward().unwrap();
        assert_eq!(r2.event.as_ref().unwrap().msg, "m20");
        // Switch to a filter that excludes m30; m40 should be next.
        let filter: Filter = "msg!=m30".parse().unwrap();
        stepper.set_filter(filter);
        let r3 = stepper.step_forward().unwrap();
        assert_eq!(r3.event.as_ref().unwrap().msg, "m40");
        // No more forward records.
        assert!(stepper.step_forward().is_none());
        dir.cleanup();
    }

    #[test]
    fn source_id_filter_excludes_whole_source() {
        // A `source_id!~b` filter at construction must keep source A's
        // events and skip source B entirely — even if B would
        // otherwise produce events that interleave by time.
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        write_fixture(&a, "x", &[10, 30]);
        write_fixture(&b, "x", &[20, 40]);
        let sa: Arc<dyn Source> = Arc::new(FileSource::open(&a).unwrap());
        let sb: Arc<dyn Source> = Arc::new(FileSource::open(&b).unwrap());
        let id_b = sb.id().clone();
        let sources = [sa, sb];
        // Build an Engine-style filter directly; the stepper applies
        // the source-id selection itself when constructed via
        // `Engine::stepper`, so we mimic that here.
        let filter: Filter =
            format!("source_id!~{}", regex::escape(id_b.as_ref()))
                .parse()
                .unwrap();
        let selected: Vec<Arc<dyn Source>> = sources
            .iter()
            .filter(|s| filter.matches_source_id(s.id()))
            .map(Arc::clone)
            .collect();
        let mut stepper = Stepper::new(selected, filter, &Cursor::new());
        assert_eq!(forward_msgs(&mut stepper), vec!["m10", "m30"]);
        dir.cleanup();
    }

    #[test]
    fn parse_errors_emit_inline() {
        // A non-JSON line in the middle of the file appears as an
        // inline error; the surrounding events still come through.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan_at(&p, "x", t(10), "m10");
        append_raw(&p, "not json");
        append_bunyan_at(&p, "x", t(20), "m20");
        let src: Arc<dyn Source> = Arc::new(FileSource::open(&p).unwrap());
        let sources = vec![src];
        let mut stepper = make_stepper(&sources);
        let r1 = stepper.step_forward().unwrap();
        assert_eq!(r1.event.as_ref().unwrap().msg, "m10");
        let r2 = stepper.step_forward().unwrap();
        assert!(r2.event.is_err());
        let r3 = stepper.step_forward().unwrap();
        assert_eq!(r3.event.as_ref().unwrap().msg, "m20");
        assert!(stepper.step_forward().is_none());
        dir.cleanup();
    }

    #[test]
    fn parse_errors_replay_on_backward_step() {
        // Forward over an error; backward must produce the error in
        // its original position rather than skipping over it.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan_at(&p, "x", t(10), "m10");
        append_raw(&p, "not json");
        append_bunyan_at(&p, "x", t(20), "m20");
        let src: Arc<dyn Source> = Arc::new(FileSource::open(&p).unwrap());
        let sources = vec![src];
        let mut stepper = make_stepper(&sources);
        // Drain forward.
        for _ in 0..3 {
            stepper.step_forward().unwrap();
        }
        // Step backward: m20, then error, then m10.
        let r1 = stepper.step_backward().unwrap();
        assert_eq!(r1.event.as_ref().unwrap().msg, "m20");
        let r2 = stepper.step_backward().unwrap();
        assert!(r2.event.is_err());
        let r3 = stepper.step_backward().unwrap();
        assert_eq!(r3.event.as_ref().unwrap().msg, "m10");
        assert!(stepper.step_backward().is_none());
        dir.cleanup();
    }

    #[test]
    fn many_records_force_multiple_batch_fetches() {
        // Write more records than `FETCH_BATCH_SIZE` so the stepper
        // must refill at least twice.  A counting source wraps the
        // underlying file source and asserts the stepper isn't
        // single-fetching everything.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        let n = (FETCH_BATCH_SIZE * 3 + 5) as i64;
        let secs: Vec<i64> = (0..n).collect();
        write_fixture(&p, "x", &secs);
        let inner = FileSource::open(&p).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let src: Arc<dyn Source> =
            Arc::new(CountingSource { inner, count: counter.clone() });
        let sources = vec![src];
        let mut stepper = make_stepper(&sources);
        let msgs = forward_msgs(&mut stepper);
        assert_eq!(msgs.len(), n as usize);
        let calls = counter.load(Ordering::SeqCst);
        // We should see at least `ceil(n / FETCH_BATCH_SIZE)` query
        // calls; a single huge fetch would be only 1.
        let expected_min = (n as usize).div_ceil(FETCH_BATCH_SIZE);
        assert!(
            calls >= expected_min,
            "expected at least {expected_min} fetches, got {calls}",
        );
        dir.cleanup();
    }

    #[test]
    fn buffer_trim_preserves_navigation_correctness() {
        // Walk far enough forward that the lookbehind buffer trims its
        // oldest entries, then walk all the way back to byte zero.
        // Trimming must not corrupt the cursor — the backward walk
        // should hit every record, refetching from disk for the
        // trimmed range.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        let n = (BUFFER_LIMIT + 10) as i64;
        let secs: Vec<i64> = (0..n).collect();
        write_fixture(&p, "x", &secs);
        let src: Arc<dyn Source> = Arc::new(FileSource::open(&p).unwrap());
        let sources = vec![src];
        let mut stepper = make_stepper(&sources);
        // Forward all the way.
        let fwd = forward_msgs(&mut stepper);
        assert_eq!(fwd.len(), n as usize);
        // Backward all the way — even though the trim has occurred.
        let bwd = backward_msgs(&mut stepper);
        let mut fwd_rev = fwd.clone();
        fwd_rev.reverse();
        assert_eq!(bwd, fwd_rev);
        dir.cleanup();
    }

    #[test]
    fn step_backward_after_eof_works() {
        // After step_forward returns None (forward exhausted), the
        // backward direction is still available.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        write_fixture(&p, "x", &[10, 20]);
        let src: Arc<dyn Source> = Arc::new(FileSource::open(&p).unwrap());
        let sources = vec![src];
        let mut stepper = make_stepper(&sources);
        forward_msgs(&mut stepper);
        // We're at EOF.
        assert!(stepper.step_forward().is_none());
        // Backward still works.
        let r = stepper.step_backward().unwrap();
        assert_eq!(r.event.as_ref().unwrap().msg, "m20");
        dir.cleanup();
    }

    #[test]
    fn merge_record_carries_offset_and_length() {
        // The (offset, length) pair on each emitted record must be
        // consistent: stepping forward through the file, the offsets
        // partition the file with no gaps.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        write_fixture(&p, "x", &[10, 20, 30]);
        let len = std::fs::metadata(&p).unwrap().len();
        let src: Arc<dyn Source> = Arc::new(FileSource::open(&p).unwrap());
        let sources = vec![src];
        let mut stepper = make_stepper(&sources);
        let mut expected_offset = ByteOffset::ZERO;
        while let Some(r) = stepper.step_forward() {
            assert_eq!(r.offset, expected_offset);
            expected_offset += r.length;
        }
        assert_eq!(expected_offset.get(), len);
        dir.cleanup();
    }

    #[test]
    fn budgeted_step_preserves_time_order_with_mid_scan_sibling() {
        // Regression test: with `max_records_to_scan_per_fill` set,
        // a `step()` that surfaces a head from source A while
        // source B is still mid-scan must not emit A's record —
        // B might surface an earlier record on a later fill.
        //
        // A holds one match at t=200; B holds 20 noise records
        // (filtered out) at t=1..=20 followed by one match at
        // t=21.  Budget of 5 records per fill means B needs
        // several fills to reach its match.  Correct order is
        // B's match first (t=21), then A's match (t=200).
        let dir = TestDir::new();
        let a_path = dir.path().join("a.log");
        let b_path = dir.path().join("b.log");
        append_bunyan_at(&a_path, "x", t(200), "match-a");
        for i in 1..=20 {
            append_bunyan_at(&b_path, "x", t(i), "noise");
        }
        append_bunyan_at(&b_path, "x", t(21), "match-b");
        let sa: Arc<dyn Source> = Arc::new(FileSource::open(&a_path).unwrap());
        let sb: Arc<dyn Source> = Arc::new(FileSource::open(&b_path).unwrap());
        let sources = vec![sa, sb];
        let filter: Filter = "msg!=noise".parse().unwrap();
        let mut stepper = Stepper::with_options(
            sources,
            filter,
            &Cursor::new(),
            StepperOptions {
                batch_size: FETCH_BATCH_SIZE,
                max_records_to_scan_per_fill: Some(5),
            },
        );
        let mut msgs = Vec::new();
        loop {
            match stepper.step_forward() {
                Some(r) => msgs.push(r.event.unwrap().msg),
                None => {
                    if stepper.is_exhausted(Direction::Forward) {
                        break;
                    }
                }
            }
        }
        assert_eq!(msgs, vec!["match-b", "match-a"]);
        dir.cleanup();
    }

    /// Wraps a `FileSource` and counts every call to `query`.  Used by
    /// the batch-fetch test to confirm the stepper actually issues
    /// multiple smaller fetches rather than reading the whole file at
    /// once.
    struct CountingSource {
        inner: FileSource,
        count: Arc<AtomicUsize>,
    }

    impl Source for CountingSource {
        fn id(&self) -> &SourceId {
            self.inner.id()
        }
        fn metadata(&self) -> &crate::source::SourceMetadata {
            self.inner.metadata()
        }
        fn events<'a>(
            &'a self,
        ) -> Box<dyn Iterator<Item = (u64, Result<Event, SourceError>)> + 'a>
        {
            self.inner.events()
        }
        fn query_bounded(
            &self,
            offset: ByteOffset,
            direction: Direction,
            count: usize,
            max_records_to_scan: Option<usize>,
            filter: &Filter,
        ) -> std::io::Result<crate::source::QueryBatch> {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.inner.query_bounded(
                offset,
                direction,
                count,
                max_records_to_scan,
                filter,
            )
        }
        fn byte_len(&self) -> std::io::Result<u64> {
            self.inner.byte_len()
        }
    }
}
