// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Engine: the library-level entry point used by both `seeit` and `seer`.
//!
//! The engine owns the set of [`Source`]s currently in play and exposes
//! queries over them.  Multi-source queries are time-merged: each source
//! is assumed to be locally sorted by [`Event::time`] (when it is not,
//! the engine emits a one-shot [`SourceError::OutOfOrder`] warning for
//! that source and proceeds anyway).  Lazy access and richer queries
//! will land here as the design develops.

use crate::event::Event;
use crate::filter::Filter;
use crate::source::{ByteOffset, FileSource, Source, SourceError, SourceId};
use crate::stream::LogStreamPosition;
use camino::Utf8Path;
use chrono::{DateTime, Utc};
use std::collections::VecDeque;

mod merge;

pub use merge::{Cursor, MergeError, MergeRecord, Stepper};

/// An [`Event`] paired with the [`LogStreamPosition`] identifying which
/// source it came from and its position within that source.
///
/// The position is a stable anchor (source id + timestamp + a tiebreaker
/// for events that share a timestamp) and survives changes to the active
/// filter — see [`LogStreamPosition`].
#[derive(Debug, Clone)]
pub struct EngineEvent {
    pub position: LogStreamPosition,
    pub event: Event,
}

/// Tracks the current set of sources and serves event queries.
#[derive(Default)]
pub struct Engine {
    sources: Vec<Box<dyn Source>>,
}

impl Engine {
    /// Returns a new engine with no sources.
    pub fn new() -> Self {
        Self { sources: Vec::new() }
    }

    /// Adds a file-backed source at `path` and returns its assigned
    /// [`SourceId`] (derived from the canonicalized path).
    pub fn add_file_source(
        &mut self,
        path: &Utf8Path,
    ) -> std::io::Result<SourceId> {
        let source = FileSource::open(path)?;
        let id = source.id().clone();
        self.sources.push(Box::new(source));
        Ok(id)
    }

    /// Constructs a [`Stepper`] over every attached source whose id is
    /// accepted by `filter`'s source-id predicates, with each source's
    /// initial byte offset taken from `cursor` (defaulting to
    /// [`crate::source::ByteOffset::ZERO`] for sources missing from the
    /// cursor).
    ///
    /// Unlike [`Self::query_events`], a stepper fetches lazily and keeps
    /// per-source lookahead/lookbehind buffers so the TUI can scroll
    /// forward and backward without re-reading the whole file each
    /// time.  Filter changes are applied via [`Stepper::set_filter`]
    /// (which retains positions); changing the *source-id* filter
    /// requires building a fresh stepper, since the source set is
    /// fixed at construction.
    pub fn stepper(&self, filter: Filter, cursor: &Cursor) -> Stepper<'_> {
        let sources: Vec<&dyn Source> = self
            .sources
            .iter()
            .filter(|s| filter.matches_source_id(s.id()))
            .map(|s| s.as_ref())
            .collect();
        Stepper::new(sources, filter, cursor)
    }

    /// Like [`Self::stepper`] but lets the caller override the
    /// per-fill batch size that the stepper hands to the storage
    /// layer.  The TUI's long-op driver passes a small value (see
    /// [`crate::engine::merge::LONG_OP_BATCH_SIZE`]) so each `query`
    /// walks only a bounded number of records before yielding —
    /// without it, a single fill under a selective filter can freeze
    /// the UI for hundreds of milliseconds at a time.
    pub fn stepper_with_batch(
        &self,
        filter: Filter,
        cursor: &Cursor,
        batch_size: usize,
    ) -> Stepper<'_> {
        let sources: Vec<&dyn Source> = self
            .sources
            .iter()
            .filter(|s| filter.matches_source_id(s.id()))
            .map(|s| s.as_ref())
            .collect();
        Stepper::with_batch_size(sources, filter, cursor, batch_size)
    }

    /// Like [`Self::stepper_with_batch`] but also lets the caller
    /// impose a per-fill records-walked budget.  The long-op driver
    /// uses this to cap the wall time spent inside a single
    /// `stepper.step` call under selective filters that have to walk
    /// many on-disk records per match.
    pub fn stepper_with_bounds(
        &self,
        filter: Filter,
        cursor: &Cursor,
        batch_size: usize,
        max_walks_per_fill: usize,
    ) -> Stepper<'_> {
        let sources: Vec<&dyn Source> = self
            .sources
            .iter()
            .filter(|s| filter.matches_source_id(s.id()))
            .map(|s| s.as_ref())
            .collect();
        Stepper::with_bounds(
            sources,
            filter,
            cursor,
            batch_size,
            Some(max_walks_per_fill),
        )
    }

    /// Sum of `byte_len()` over every source whose id is accepted by
    /// `filter`'s source-id predicates.  Used as the denominator for a
    /// progress bar over a full-file pass: a long-running operation
    /// (Summary build, search across an unmatched file) can divide its
    /// running [`EventStream::bytes_read`] by this to drive a percentage.
    /// Sources whose `byte_len()` syscall fails contribute zero — the
    /// progress will overshoot rather than fail outright, which is the
    /// less surprising behavior here.
    pub fn filtered_total_bytes(&self, filter: &Filter) -> u64 {
        self.sources
            .iter()
            .filter(|s| filter.matches_source_id(s.id()))
            .map(|s| s.byte_len().unwrap_or(0))
            .sum()
    }

    /// Returns a [`Cursor`] positioned at end-of-file for every source
    /// the engine knows about (regardless of whether `filter` accepts
    /// the source — the cursor includes all sources so it survives
    /// later filter changes).  Suitable for "jump to end" navigation:
    /// `engine.stepper(filter, &cursor)`'s `step_forward` returns
    /// `None`, while `step_backward` walks the merged stream in
    /// reverse from the latest event.
    pub fn cursor_at_end(&self) -> std::io::Result<Cursor> {
        let mut cursor = Cursor::new();
        for s in &self.sources {
            cursor.set(s.id().clone(), s.byte_len()?.into());
        }
        Ok(cursor)
    }

    /// Walks the unfiltered merge to find the event at `position` and
    /// returns a [`Cursor`] just *before* that event — i.e., a cursor
    /// such that `engine.stepper(filter, &cursor)`'s next `step_forward`
    /// returns `position`'s event itself.
    ///
    /// "Before" rather than "after" is the semantic the navigation
    /// callers want: feeding the result to [`crate::StreamView::seek_to_cursor`]
    /// places the viewport's anchor on the bookmarked record.
    ///
    /// Returns `None` when `position`'s source isn't attached to this
    /// engine, or when no event in that source matches the position's
    /// time/ordinal pair.  Does not consult any filter: bookmark
    /// resolution must work even when the active filter excludes the
    /// bookmarked event.
    pub fn cursor_for_position(
        &self,
        position: &LogStreamPosition,
    ) -> Option<Cursor> {
        if !self.sources.iter().any(|s| s.id() == position.source()) {
            return None;
        }
        // Walk the unfiltered merge.  We accumulate per-source offsets
        // in a cursor as we see each event; when we hit the anchor we
        // return *before* updating, so the returned cursor names "just
        // before" the bookmarked event.  Other sources' offsets in the
        // cursor reflect the latest event we've seen from them prior
        // to the bookmark — which is exactly what merging from this
        // cursor wants.
        let mut cursor = Cursor::new();
        for s in &self.sources {
            cursor.set(s.id().clone(), ByteOffset::ZERO);
        }
        let unfiltered = Filter::default();
        let stepper_sources: Vec<&dyn Source> =
            self.sources.iter().map(|s| s.as_ref()).collect();
        let mut stepper =
            Stepper::new(stepper_sources, unfiltered, &Cursor::new());
        let mut ordinal_seen: u64 = 0;
        let mut in_same_time_group = false;
        while let Some(rec) = stepper.step_forward() {
            let Ok(event) = rec.event else {
                // Per-line errors don't carry a time/source pair the
                // bookmark could be anchored to; advance the cursor
                // past them and continue.
                cursor.set(
                    rec.source_id.clone(),
                    ByteOffset::from(rec.offset.get() + rec.length),
                );
                continue;
            };
            let on_target_group = rec.source_id == *position.source()
                && event.time == position.time();
            if on_target_group {
                if !in_same_time_group {
                    in_same_time_group = true;
                    ordinal_seen = 0;
                }
                if ordinal_seen == position.ordinal_within_time() {
                    return Some(cursor);
                }
                ordinal_seen += 1;
            } else if in_same_time_group {
                // We walked off the matching group without finding
                // the requested ordinal.
                return None;
            }
            cursor.set(
                rec.source_id.clone(),
                ByteOffset::from(rec.offset.get() + rec.length),
            );
        }
        None
    }

    /// Returns an iterator over every event in every source that
    /// matches `filter`.
    ///
    /// Sources whose ids fail the filter's source-id predicates are
    /// skipped entirely — the engine never opens them or iterates
    /// their events, which is the whole point of having a source-id
    /// predicate (a regex over an absolute path can prune a multi-GB
    /// log file before any reads).  The remaining sources' events are
    /// interleaved by time so the caller sees the full timeline, not
    /// one source at a time.  Each source is assumed to be locally
    /// sorted; if a source has an out-of-order entry, a one-shot
    /// [`SourceError::OutOfOrder`] warning for that source is emitted
    /// *before* the offending event (and the merge continues with the
    /// timestamps as given).  Per-line parse and I/O errors appear
    /// inline as `Err` items, kept close to their position in the
    /// source file rather than being slotted by time.  The filter only
    /// applies to `Ok` items, so errors and warnings are always
    /// surfaced regardless of the filter.
    pub fn query_events<'a>(&'a self, filter: &'a Filter) -> EventStream<'a> {
        let cursors = self
            .sources
            .iter()
            .filter(|s| filter.matches_source_id(s.id()))
            .map(|s| SourceCursor::new(s.as_ref()))
            .collect();
        EventStream { cursors, filter, records_parsed: 0 }
    }

    /// Resolves `position` against the current sources, viewed through
    /// `filter`, into a row index in the corresponding
    /// `query_events(filter)` output.
    ///
    /// Bookmarks anchor by `(source, time, ordinal_within_time)` rather
    /// than by ordinal in a filtered view, so this method has to walk
    /// the unfiltered stream to find the anchored event and then count
    /// how many filter-matching events precede it.  See
    /// [`ResolvePosition`] for the meaning of each outcome.
    pub fn resolve_position(
        &self,
        filter: &Filter,
        position: &LogStreamPosition,
    ) -> ResolvePosition {
        if !self.sources.iter().any(|s| s.id() == position.source()) {
            return ResolvePosition::Gone;
        }
        // The anchor's source might still be attached but excluded by
        // the source-id filter — in that case the merge never sees the
        // anchor at all.  Handle this up front by finding the closest
        // visible neighbor by *time*, matching the FilteredOut
        // contract (prefer next-after, fall back to previous-before).
        if !filter.matches_source_id(position.source()) {
            return self.filtered_out_neighbor_by_time(filter, position.time());
        }
        let mut row_in_filtered: usize = 0;
        let mut last_visible_before: Option<usize> = None;
        // Walk the unfiltered merge.  For each Ok event, decide three
        // things: is it the anchor (compare positions); is it visible
        // under `filter`; is it earlier or later than the anchor in
        // emit order.  Errors don't appear in `query_events(filter)`
        // either when filter rejects no errors — but our Ok-only counter
        // is what matters here, since callers index into events, and
        // their events vec parallels query_events output.
        //
        // Sources whose ids fail the source-id filter are excluded from
        // the merge, mirroring `query_events`.  The anchor's source has
        // already been confirmed visible above, so the anchor (if it
        // still exists in the file) will be reached.
        let cursors = self
            .sources
            .iter()
            .filter(|s| filter.matches_source_id(s.id()))
            .map(|s| SourceCursor::new(s.as_ref()))
            .collect();
        let merge = MergeIter { cursors };
        // We need to know if the anchor exists at all (to distinguish
        // FilteredOut from Gone) and where the next visible event after
        // the anchor sits.  Track these through a second pass over the
        // tail.  Concretely: walk in one pass and react when we hit
        // (or pass) the anchor.
        let mut found_anchor = false;
        let mut next_visible_after: Option<usize> = None;
        for item in merge {
            let Ok(ee) = item else { continue };
            let visible = filter.matches(&ee.event);
            if found_anchor {
                if visible {
                    next_visible_after = Some(row_in_filtered);
                    break;
                }
                // Each filter-matched Ok event would advance the index;
                // non-matched events are skipped, matching query_events.
                continue;
            }
            if &ee.position == position {
                found_anchor = true;
                if visible {
                    return ResolvePosition::Found(row_in_filtered);
                }
                // Anchor exists but is filtered out; next visible event
                // (if any) wins.  Don't advance row_in_filtered for a
                // filtered-out event.
                continue;
            }
            if visible {
                last_visible_before = Some(row_in_filtered);
                row_in_filtered += 1;
            }
        }
        if !found_anchor {
            // Source is visible to the filter (the !visible branch
            // returned earlier) but the anchor isn't in the merge —
            // the underlying file has been rewritten or rotated since
            // the bookmark was made.
            return ResolvePosition::Gone;
        }
        // Anchor is present but filtered out; pick the closest visible
        // neighbor.  Prefer the next event after the anchor (closer in
        // time when scrolling forward through bookmarks); fall back to
        // the previous event when nothing later is visible.
        match (next_visible_after, last_visible_before) {
            (Some(idx), _) => ResolvePosition::FilteredOut(idx),
            (None, Some(idx)) => ResolvePosition::FilteredOut(idx),
            (None, None) => ResolvePosition::FilteredOut(0),
        }
    }

    /// Closest visible neighbor of an anchor whose source was excluded
    /// by the source-id filter, indexed by time alone.
    ///
    /// Walks the source-id-filtered merge once and returns:
    /// - the row of the first visible event whose `time > anchor_time`,
    /// - or, when no later event is visible, the row of the latest
    ///   visible event whose `time <= anchor_time`,
    /// - or `0` when nothing is visible at all.
    ///
    /// Mirrors the next-then-previous preference of the in-merge
    /// FilteredOut path so a bookmark whose source is regex-excluded
    /// behaves the same as one whose anchored event was excluded by an
    /// event-level predicate.
    fn filtered_out_neighbor_by_time(
        &self,
        filter: &Filter,
        anchor_time: DateTime<Utc>,
    ) -> ResolvePosition {
        let cursors = self
            .sources
            .iter()
            .filter(|s| filter.matches_source_id(s.id()))
            .map(|s| SourceCursor::new(s.as_ref()))
            .collect();
        let mut row = 0usize;
        let mut last_before: Option<usize> = None;
        for item in (MergeIter { cursors }) {
            let Ok(ee) = item else { continue };
            if !filter.matches(&ee.event) {
                continue;
            }
            if ee.event.time > anchor_time {
                return ResolvePosition::FilteredOut(row);
            }
            last_before = Some(row);
            row += 1;
        }
        ResolvePosition::FilteredOut(last_before.unwrap_or(0))
    }
}

/// Outcome of [`Engine::resolve_position`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvePosition {
    /// The bookmarked event is currently visible at the returned row
    /// index in `query_events(filter)`.
    Found(usize),
    /// The bookmarked event still exists in the source but is hidden by
    /// the active filter.  The returned index is the closest visible
    /// neighbor: preferring the next visible event after the anchor,
    /// falling back to the previous one when nothing later survives the
    /// filter.  When no events at all survive the filter the index is
    /// `0`, which is conventionally the top of the (empty) view.
    FilteredOut(usize),
    /// The source the bookmark refers to is no longer attached to this
    /// engine, or the source is attached but the anchored event isn't
    /// present (e.g. the file was rewritten between sessions).
    Gone,
}

/// One-step lookahead over a single [`Source`]'s event stream, with
/// out-of-order detection.
///
/// `pending` holds the next item(s) the merge will see.  Normally that
/// is exactly one item — the head — but when an out-of-order regression
/// is detected the cursor pushes a synthetic
/// [`SourceError::OutOfOrder`] *before* the offending event, so the
/// queue briefly holds two.  `last_time` is the timestamp of the most
/// recent `Ok` event, used to detect regressions.  `out_of_order_warned`
/// makes the warning one-shot per source: a wildly unsorted file
/// shouldn't drown its real entries in repeated warnings.
struct SourceCursor<'a> {
    iter: Box<dyn Iterator<Item = (u64, Result<Event, SourceError>)> + 'a>,
    pending: VecDeque<Result<EngineEvent, SourceError>>,
    last_time: Option<DateTime<Utc>>,
    /// Number of events already emitted whose `time` matches `last_time`.
    /// Used to compute the next event's intra-time ordinal so two events
    /// with identical timestamps still have distinct
    /// [`LogStreamPosition`]s.  Resets to 0 whenever the timestamp
    /// changes.
    intra_time_count: u64,
    source_id: SourceId,
    out_of_order_warned: bool,
    /// Total source bytes pulled off this source so far, including
    /// bytes from parse-error lines and (when present) line
    /// terminators.  Summed across cursors to drive the
    /// [`EventStream::bytes_read`] accessor.
    bytes_read: u64,
}

impl<'a> SourceCursor<'a> {
    fn new(source: &'a dyn Source) -> Self {
        Self {
            iter: source.events(),
            pending: VecDeque::new(),
            last_time: None,
            intra_time_count: 0,
            source_id: source.id().clone(),
            out_of_order_warned: false,
            bytes_read: 0,
        }
    }

    /// Ensures `pending` has at least one item if the underlying
    /// iterator hasn't been exhausted.  When the next item is an event
    /// whose timestamp regresses, prepends a one-shot
    /// `OutOfOrder` warning ahead of it.
    fn fill(&mut self) {
        if !self.pending.is_empty() {
            return;
        }
        let Some((bytes, item)) = self.iter.next() else {
            return;
        };
        self.bytes_read += bytes;
        match item {
            Err(e) => {
                self.pending.push_back(Err(e));
            }
            Ok(event) => {
                if let Some(prev) = self.last_time
                    && event.time < prev
                    && !self.out_of_order_warned
                {
                    self.pending.push_back(Err(SourceError::OutOfOrder {
                        source_id: self.source_id.clone(),
                        seen: event.time,
                        last_seen: prev,
                    }));
                    self.out_of_order_warned = true;
                }
                let ordinal = if self.last_time == Some(event.time) {
                    self.intra_time_count
                } else {
                    0
                };
                self.intra_time_count = ordinal + 1;
                self.last_time = Some(event.time);
                let position = LogStreamPosition::new(
                    self.source_id.clone(),
                    event.time,
                    ordinal,
                );
                self.pending.push_back(Ok(EngineEvent { position, event }));
            }
        }
    }

    /// Returns the head of the cursor without consuming it.  Returns
    /// `None` once the source is fully drained.
    fn peek(&mut self) -> Option<&Result<EngineEvent, SourceError>> {
        self.fill();
        self.pending.front()
    }

    /// Consumes and returns the head of the cursor.
    fn pop(&mut self) -> Option<Result<EngineEvent, SourceError>> {
        self.fill();
        self.pending.pop_front()
    }
}

/// Streaming output of [`Engine::query_events`].
///
/// Wraps the per-source cursors plus the active filter, and surfaces
/// running parse statistics that the TUI uses to render its
/// "N records (M MiB) parsed in T..." status row.  The counters reflect
/// what has been pulled so far; for the final totals, drain the
/// iterator first and then read the accessors.
///
/// The event-level filter is applied here (errors and warnings always
/// pass through); the source-id filter is applied at construction by
/// only creating cursors for matching sources.
pub struct EventStream<'a> {
    cursors: Vec<SourceCursor<'a>>,
    filter: &'a Filter,
    records_parsed: u64,
}

impl<'a> EventStream<'a> {
    /// Total number of `Ok` events produced by the underlying sources
    /// so far, regardless of whether the active event filter accepted
    /// them.  Counts events that came off disk and parsed cleanly —
    /// the natural denominator for "records parsed per second".
    pub fn records_parsed(&self) -> u64 {
        self.records_parsed
    }

    /// Total source bytes consumed across all sources so far,
    /// including bytes for parse-error lines and (when present) line
    /// terminators.
    pub fn bytes_read(&self) -> u64 {
        self.cursors.iter().map(|c| c.bytes_read).sum()
    }
}

impl<'a> Iterator for EventStream<'a> {
    type Item = Result<EngineEvent, SourceError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let item = pop_next(&mut self.cursors)?;
            if item.is_ok() {
                self.records_parsed += 1;
            }
            if let Ok(ee) = &item
                && !self.filter.matches(&ee.event)
            {
                continue;
            }
            return Some(item);
        }
    }
}

/// K-way merge over a `Vec<SourceCursor>`.
///
/// On each `next` call: scan all cursors and pick the one to emit from.
/// A cursor whose head is a non-event (parse/IO error or out-of-order
/// warning) is preferred over any cursor whose head is an event,
/// regardless of time, so error and warning rows stay close to their
/// position in the file that produced them.  Among event heads, the
/// smallest timestamp wins.  All ties (errors-with-errors,
/// equal-timestamp events) break by source-add order.
struct MergeIter<'a> {
    cursors: Vec<SourceCursor<'a>>,
}

impl<'a> Iterator for MergeIter<'a> {
    type Item = Result<EngineEvent, SourceError>;

    fn next(&mut self) -> Option<Self::Item> {
        pop_next(&mut self.cursors)
    }
}

/// Pops the next item across `cursors` using the rule documented on
/// [`MergeIter`]: error/warning heads are emitted ahead of any event
/// head; among event heads, smallest timestamp wins; ties break by
/// add-order.  Returns `None` once every cursor is drained.
fn pop_next<'a>(
    cursors: &mut [SourceCursor<'a>],
) -> Option<Result<EngineEvent, SourceError>> {
    let mut best: Option<usize> = None;
    let mut best_time: Option<DateTime<Utc>> = None;
    let mut best_is_err = false;

    // Index-based loop: each iteration mutably borrows a different
    // cursor (peek takes `&mut self`), which `iter_mut().enumerate()`
    // can't express without splitting the slice.
    #[allow(clippy::needless_range_loop)]
    for i in 0..cursors.len() {
        // Decide based on this cursor's head, then drop the borrow so
        // the next iteration can mutably borrow another cursor.
        let outcome = match cursors[i].peek() {
            None => None,
            Some(Err(_)) => Some((true, None)),
            Some(Ok(ev)) => Some((false, Some(ev.event.time))),
        };
        let Some((is_err, time)) = outcome else {
            continue;
        };
        if is_err {
            if !best_is_err {
                best = Some(i);
                best_is_err = true;
            }
            // Earlier source already wins on err-vs-err tie.
            continue;
        }
        if best_is_err {
            continue;
        }
        let t = time.expect("non-error head has a time");
        if best_time.is_none_or(|bt| t < bt) {
            best = Some(i);
            best_time = Some(t);
        }
    }

    let idx = best?;
    cursors[idx].pop()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Level;
    use crate::test_fixtures::{
        TestDir, append_bunyan, append_bunyan_at, append_raw, t,
    };
    use slog::{debug, error, info};

    #[test]
    fn filtered_total_bytes_sums_byte_lens_of_matching_sources() {
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        append_bunyan_at(&a, "Nexus", t(10), "a1");
        append_bunyan_at(&b, "SledAgent", t(20), "b1");

        let mut engine = Engine::new();
        engine.add_file_source(&a).unwrap();
        engine.add_file_source(&b).unwrap();

        let total = engine.filtered_total_bytes(&Filter::default());
        let a_size = std::fs::metadata(&a).unwrap().len();
        let b_size = std::fs::metadata(&b).unwrap().len();
        assert_eq!(total, a_size + b_size);

        // A source-id predicate that excludes `b` drops `b`'s bytes
        // from the denominator; a real progress bar driven by this
        // value would track only the sources that will actually be
        // scanned.
        let path_filter: Filter = "source_id!~b\\.log".parse().unwrap();
        assert_eq!(engine.filtered_total_bytes(&path_filter), a_size);

        dir.cleanup();
    }

    #[test]
    fn filtered_total_bytes_empty_engine_is_zero() {
        let engine = Engine::new();
        assert_eq!(engine.filtered_total_bytes(&Filter::default()), 0);
    }

    #[test]
    fn query_merges_sources_in_time_order() {
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        append_bunyan_at(&a, "x", t(10), "a1");
        append_bunyan_at(&a, "x", t(30), "a2");
        append_bunyan_at(&b, "x", t(20), "b1");
        append_bunyan_at(&b, "x", t(40), "b2");

        let mut engine = Engine::new();
        let id_a = engine.add_file_source(&a).unwrap();
        let id_b = engine.add_file_source(&b).unwrap();
        assert_ne!(id_a, id_b);

        let filter = Filter::default();
        let msgs: Vec<_> = engine
            .query_events(&filter)
            .map(|e| e.unwrap().event.msg)
            .collect();
        assert_eq!(msgs, vec!["a1", "b1", "a2", "b2"]);

        dir.cleanup();
    }

    #[test]
    fn query_breaks_ties_by_source_add_order() {
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        // Both events are at the same instant; the source added first
        // should be emitted first.
        append_bunyan_at(&a, "x", t(50), "a-tie");
        append_bunyan_at(&b, "x", t(50), "b-tie");

        let mut engine = Engine::new();
        engine.add_file_source(&a).unwrap();
        engine.add_file_source(&b).unwrap();
        let msgs: Vec<_> = engine
            .query_events(&Filter::default())
            .map(|e| e.unwrap().event.msg)
            .collect();
        assert_eq!(msgs, vec!["a-tie", "b-tie"]);

        dir.cleanup();
    }

    #[test]
    fn query_skips_empty_source_in_merge() {
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        // Empty source between two populated ones.
        std::fs::File::create(&b).unwrap();
        append_bunyan_at(&a, "x", t(10), "a1");
        append_bunyan_at(&a, "x", t(20), "a2");

        let mut engine = Engine::new();
        engine.add_file_source(&a).unwrap();
        engine.add_file_source(&b).unwrap();
        let msgs: Vec<_> = engine
            .query_events(&Filter::default())
            .map(|e| e.unwrap().event.msg)
            .collect();
        assert_eq!(msgs, vec!["a1", "a2"]);

        dir.cleanup();
    }

    #[test]
    fn query_keeps_errors_near_source_position() {
        // Source A:  t=10, parse_err, t=30
        // Source B:  t=20
        // Expected:  A's t=10 (smallest time wins),
        //            A's parse error (head is err -> emitted eagerly,
        //              regardless of B's smaller-time head),
        //            B's t=20,
        //            A's t=30.
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        append_bunyan_at(&a, "x", t(10), "a1");
        append_raw(&a, "not json");
        append_bunyan_at(&a, "x", t(30), "a2");
        append_bunyan_at(&b, "x", t(20), "b1");

        let mut engine = Engine::new();
        engine.add_file_source(&a).unwrap();
        engine.add_file_source(&b).unwrap();
        let results: Vec<_> = engine.query_events(&Filter::default()).collect();
        assert_eq!(results.len(), 4);
        assert_eq!(results[0].as_ref().unwrap().event.msg, "a1");
        assert!(matches!(
            results[1].as_ref().unwrap_err(),
            SourceError::Parse(_),
        ));
        assert_eq!(results[2].as_ref().unwrap().event.msg, "b1");
        assert_eq!(results[3].as_ref().unwrap().event.msg, "a2");

        dir.cleanup();
    }

    #[test]
    fn query_warns_when_source_out_of_order() {
        let dir = TestDir::new();
        let p = dir.path().join("unsorted.log");
        append_bunyan_at(&p, "x", t(10), "first");
        append_bunyan_at(&p, "x", t(30), "second");
        // Regression: t=20 follows t=30.
        append_bunyan_at(&p, "x", t(20), "third");
        // Another regression that should NOT trigger a second warning.
        append_bunyan_at(&p, "x", t(15), "fourth");

        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let results: Vec<_> = engine.query_events(&Filter::default()).collect();

        // Output: first, second, [warning], third, fourth.  The warning
        // is emitted just before the first regressing event, and only
        // once for the source.
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].as_ref().unwrap().event.msg, "first");
        assert_eq!(results[1].as_ref().unwrap().event.msg, "second");
        let warning = results[2].as_ref().unwrap_err();
        match warning {
            SourceError::OutOfOrder { seen, last_seen, .. } => {
                assert_eq!(*seen, t(20));
                assert_eq!(*last_seen, t(30));
            }
            other => panic!("expected OutOfOrder, got {other:?}"),
        }
        assert_eq!(results[3].as_ref().unwrap().event.msg, "third");
        assert_eq!(results[4].as_ref().unwrap().event.msg, "fourth");

        dir.cleanup();
    }

    #[test]
    fn query_warns_independently_per_source() {
        // Two sources, each with their own regression.  Each source
        // should emit one warning, indexed by its own `source_id`.
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        append_bunyan_at(&a, "x", t(100), "a1");
        append_bunyan_at(&a, "x", t(50), "a-back"); // regression
        append_bunyan_at(&b, "x", t(200), "b1");
        append_bunyan_at(&b, "x", t(60), "b-back"); // regression

        let mut engine = Engine::new();
        let id_a = engine.add_file_source(&a).unwrap();
        let id_b = engine.add_file_source(&b).unwrap();
        let results: Vec<_> = engine.query_events(&Filter::default()).collect();
        let warnings: Vec<_> = results
            .iter()
            .filter_map(|r| match r {
                Err(SourceError::OutOfOrder { source_id, .. }) => {
                    Some(source_id.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(warnings.len(), 2);
        assert!(warnings.contains(&id_a));
        assert!(warnings.contains(&id_b));

        dir.cleanup();
    }

    #[test]
    fn out_of_order_display_includes_source_id() {
        let err = SourceError::OutOfOrder {
            source_id: SourceId::from("x.log".to_string()),
            seen: t(20),
            last_seen: t(30),
        };
        let s = err.to_string();
        assert!(s.starts_with("warning:"), "{s}");
        assert!(s.contains("x.log"), "{s}");
    }

    #[test]
    fn query_preserves_levels() {
        let dir = TestDir::new();
        let p = dir.path().join("c.log");
        append_bunyan(&p, "x", |log| {
            debug!(log, "d");
            error!(log, "e");
        });
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let filter = Filter::default();
        let levels: Vec<_> = engine
            .query_events(&filter)
            .map(|e| e.unwrap().event.level)
            .collect();
        assert_eq!(levels, vec![Level::Debug, Level::Error]);

        dir.cleanup();
    }

    #[test]
    fn query_filters_by_level() {
        let dir = TestDir::new();
        let p = dir.path().join("levels.log");
        append_bunyan(&p, "x", |log| {
            debug!(log, "d");
            info!(log, "i");
            error!(log, "e");
        });
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();

        let filter: Filter = "level>=warn".parse().unwrap();
        let msgs: Vec<_> = engine
            .query_events(&filter)
            .map(|r| r.unwrap().event.msg)
            .collect();
        assert_eq!(msgs, vec!["e"]);

        dir.cleanup();
    }

    #[test]
    fn query_filters_by_field_and_msg_regex() {
        let dir = TestDir::new();
        let p = dir.path().join("fields.log");
        append_bunyan(&p, "Nexus", |log| {
            info!(log, "blueprint executed");
            info!(log, "boot complete");
            info!(log, "blueprint failed");
        });
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();

        let filter: Filter = "name=Nexus msg=~blueprint".parse().unwrap();
        let msgs: Vec<_> = engine
            .query_events(&filter)
            .map(|r| r.unwrap().event.msg)
            .collect();
        assert_eq!(msgs, vec!["blueprint executed", "blueprint failed"]);

        dir.cleanup();
    }

    /// Build a fixture engine where each source has events at the
    /// supplied epoch seconds (one per second, message `"<name> <i>"`).
    /// Returns the engine plus the [`SourceId`] for each source so
    /// tests can build anchors.
    fn fixture_engine(
        dir: &TestDir,
        sources: &[(&str, &[i64])],
    ) -> (Engine, Vec<SourceId>) {
        let mut engine = Engine::new();
        let mut ids = Vec::new();
        for (name, seconds) in sources {
            let p = dir.path().join(format!("{name}.log"));
            for sec in *seconds {
                append_bunyan_at(&p, "x", t(*sec), &format!("{name} {sec}"));
            }
            ids.push(engine.add_file_source(&p).unwrap());
        }
        (engine, ids)
    }

    /// Walks `engine`'s default-filter merge and returns the position of
    /// the n-th `Ok` event.  Tests use this instead of building anchors
    /// by hand because `LogStreamPosition` carries a tiebreaker that
    /// would be tedious (and brittle) to mint manually.
    fn nth_position(engine: &Engine, n: usize) -> LogStreamPosition {
        engine
            .query_events(&Filter::default())
            .filter_map(|r| r.ok())
            .nth(n)
            .expect("event index in range")
            .position
    }

    #[test]
    fn resolve_position_finds_anchored_event_under_default_filter() {
        let dir = TestDir::new();
        let (engine, _ids) = fixture_engine(&dir, &[("a", &[10, 20, 30])]);
        let anchor = nth_position(&engine, 1);
        assert_eq!(
            engine.resolve_position(&Filter::default(), &anchor),
            ResolvePosition::Found(1),
        );
        dir.cleanup();
    }

    #[test]
    fn resolve_position_survives_unrelated_filter_changes() {
        // Anchor on a `name=Nexus` event then re-resolve under a
        // filter that strips every `name=Other` event.  The anchor's
        // row index should now be the first remaining row.
        let dir = TestDir::new();
        let p = dir.path().join("c.log");
        append_bunyan_at(&p, "Other", t(10), "a");
        append_bunyan_at(&p, "Nexus", t(20), "b");
        append_bunyan_at(&p, "Other", t(30), "c");
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let anchor = nth_position(&engine, 1);
        let filter: Filter = "name=Nexus".parse().unwrap();
        assert_eq!(
            engine.resolve_position(&filter, &anchor),
            ResolvePosition::Found(0),
        );
        dir.cleanup();
    }

    #[test]
    fn resolve_position_when_anchored_event_is_filtered_out_picks_next() {
        // Three events; anchor on the middle one.  Apply a filter that
        // drops only the middle one — resolution should jump to the
        // *next* still-visible row, which is the third event at row 1
        // in the filtered view.
        let dir = TestDir::new();
        let p = dir.path().join("c.log");
        append_bunyan_at(&p, "x", t(10), "a");
        append_bunyan_at(&p, "x", t(20), "b");
        append_bunyan_at(&p, "x", t(30), "c");
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let anchor = nth_position(&engine, 1);
        let filter: Filter = "msg!=b".parse().unwrap();
        assert_eq!(
            engine.resolve_position(&filter, &anchor),
            ResolvePosition::FilteredOut(1),
        );
        dir.cleanup();
    }

    #[test]
    fn resolve_position_falls_back_to_previous_when_no_later_visible() {
        // Anchor on the last event, which the filter excludes.  No
        // later visible event → fall back to the previous one.
        let dir = TestDir::new();
        let p = dir.path().join("c.log");
        append_bunyan_at(&p, "x", t(10), "a");
        append_bunyan_at(&p, "x", t(20), "b");
        append_bunyan_at(&p, "x", t(30), "tail");
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let anchor = nth_position(&engine, 2);
        let filter: Filter = "msg!=tail".parse().unwrap();
        assert_eq!(
            engine.resolve_position(&filter, &anchor),
            ResolvePosition::FilteredOut(1),
        );
        dir.cleanup();
    }

    #[test]
    fn resolve_position_when_source_is_gone() {
        // Anchor against engine A (with a source), then resolve against
        // engine B (with no sources).  The bookmark cannot find its
        // source.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan_at(&p, "x", t(10), "a");
        let mut engine_a = Engine::new();
        engine_a.add_file_source(&p).unwrap();
        let anchor = nth_position(&engine_a, 0);
        let engine_b = Engine::new();
        assert_eq!(
            engine_b.resolve_position(&Filter::default(), &anchor),
            ResolvePosition::Gone,
        );
        dir.cleanup();
    }

    #[test]
    fn query_filters_by_source_id_regex() {
        // Two sources whose canonical paths contain different basename
        // tokens.  A `source_id=~nexus` predicate must keep nexus's
        // events and drop sled-agent's, regardless of event-level
        // fields.
        let dir = TestDir::new();
        let nexus = dir.path().join("nexus.log");
        let sled = dir.path().join("sled-agent.log");
        append_bunyan_at(&nexus, "x", t(10), "n1");
        append_bunyan_at(&nexus, "x", t(20), "n2");
        append_bunyan_at(&sled, "x", t(15), "s1");
        let mut engine = Engine::new();
        engine.add_file_source(&nexus).unwrap();
        engine.add_file_source(&sled).unwrap();

        let filter: Filter = "source_id=~nexus".parse().unwrap();
        let msgs: Vec<_> = engine
            .query_events(&filter)
            .map(|r| r.unwrap().event.msg)
            .collect();
        assert_eq!(msgs, vec!["n1", "n2"]);

        let filter: Filter = "source_id!~nexus".parse().unwrap();
        let msgs: Vec<_> = engine
            .query_events(&filter)
            .map(|r| r.unwrap().event.msg)
            .collect();
        assert_eq!(msgs, vec!["s1"]);

        dir.cleanup();
    }

    #[test]
    fn source_id_filter_skips_sources_without_iterating() {
        // A source whose path can't be opened (e.g. it was deleted
        // between `add_file_source` and the query) would normally
        // surface an `Io` error from its iterator.  When the source-id
        // filter rejects that source, the engine must not even
        // construct its cursor — proven here by deleting the file
        // after registering it: an `Io` error would only appear if the
        // engine still tried to read it.
        let dir = TestDir::new();
        let nexus = dir.path().join("nexus.log");
        let agent = dir.path().join("agent.log");
        append_bunyan_at(&nexus, "x", t(10), "n1");
        append_bunyan_at(&agent, "x", t(20), "a1");
        let mut engine = Engine::new();
        engine.add_file_source(&nexus).unwrap();
        engine.add_file_source(&agent).unwrap();
        // Delete the agent file *after* it has been registered.  A
        // naive query would now surface an Io error from the agent
        // cursor; the regex must keep the engine from ever opening it.
        std::fs::remove_file(&agent).unwrap();

        let filter: Filter = "source_id=~nexus".parse().unwrap();
        let results: Vec<_> = engine.query_events(&filter).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap().event.msg, "n1");

        dir.cleanup();
    }

    #[test]
    fn source_id_filter_combines_with_event_filter() {
        // Conjunction: event must clear both the source-id regex and
        // the event-level filter.  Two sources, each with mixed
        // levels; ask for `source_id=~nexus level>=warn`.
        let dir = TestDir::new();
        let nexus = dir.path().join("nexus.log");
        let sled = dir.path().join("sled.log");
        append_bunyan(&nexus, "x", |log| {
            info!(log, "n-info");
            error!(log, "n-error");
        });
        append_bunyan(&sled, "x", |log| {
            error!(log, "s-error");
        });
        let mut engine = Engine::new();
        engine.add_file_source(&nexus).unwrap();
        engine.add_file_source(&sled).unwrap();

        let filter: Filter = "source_id=~nexus level>=warn".parse().unwrap();
        let msgs: Vec<_> = engine
            .query_events(&filter)
            .map(|r| r.unwrap().event.msg)
            .collect();
        assert_eq!(msgs, vec!["n-error"]);

        dir.cleanup();
    }

    #[test]
    fn resolve_position_treats_source_id_filter_as_filtered_out() {
        // Anchor on a source's event, then apply a filter that
        // excludes the anchor's source.  The bookmark's source is
        // still attached, so the result is FilteredOut against the
        // closest visible neighbor — not Gone.
        let dir = TestDir::new();
        let nexus = dir.path().join("nexus.log");
        let sled = dir.path().join("sled.log");
        append_bunyan_at(&nexus, "x", t(10), "n1");
        append_bunyan_at(&sled, "x", t(5), "s-before");
        append_bunyan_at(&sled, "x", t(20), "s-after");
        let mut engine = Engine::new();
        engine.add_file_source(&nexus).unwrap();
        engine.add_file_source(&sled).unwrap();
        // Anchor on the nexus event.  The merge order is
        // s-before(5), n1(10), s-after(20); n1 is index 1.
        let anchor = engine
            .query_events(&Filter::default())
            .filter_map(|r| r.ok())
            .find(|ee| ee.event.msg == "n1")
            .unwrap()
            .position;

        // Exclude nexus by source-id.  Visible-only sequence is
        // s-before(5), s-after(20); the anchor (t=10) is between them,
        // so the nearest visible-later event wins per the FilteredOut
        // contract: s-after at filtered row 1.
        let filter: Filter = "source_id!~nexus".parse().unwrap();
        assert_eq!(
            engine.resolve_position(&filter, &anchor),
            ResolvePosition::FilteredOut(1),
        );

        dir.cleanup();
    }

    #[test]
    fn resolve_position_falls_back_to_before_when_no_later_under_source_id_filter()
     {
        // Same shape as the previous test, but the anchor sits *after*
        // every other source's events.  No later visible neighbor →
        // fall back to the latest earlier visible event.
        let dir = TestDir::new();
        let nexus = dir.path().join("nexus.log");
        let sled = dir.path().join("sled.log");
        append_bunyan_at(&sled, "x", t(5), "s-1");
        append_bunyan_at(&sled, "x", t(7), "s-2");
        append_bunyan_at(&nexus, "x", t(20), "n-tail");
        let mut engine = Engine::new();
        engine.add_file_source(&nexus).unwrap();
        engine.add_file_source(&sled).unwrap();
        let anchor = engine
            .query_events(&Filter::default())
            .filter_map(|r| r.ok())
            .find(|ee| ee.event.msg == "n-tail")
            .unwrap()
            .position;

        let filter: Filter = "source_id!~nexus".parse().unwrap();
        // Visible events: s-1(0), s-2(1).  Anchor t=20 is later than
        // both, so we fall back to the latest before: row 1 (s-2).
        assert_eq!(
            engine.resolve_position(&filter, &anchor),
            ResolvePosition::FilteredOut(1),
        );

        dir.cleanup();
    }

    #[test]
    fn resolve_position_disambiguates_same_time_events() {
        // Two events from the same source share a timestamp.  Anchor
        // on the second; resolve must return its index, not the
        // first's.
        let dir = TestDir::new();
        let p = dir.path().join("c.log");
        append_bunyan_at(&p, "x", t(10), "a");
        append_bunyan_at(&p, "x", t(10), "b"); // same time
        append_bunyan_at(&p, "x", t(20), "c");
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let first = nth_position(&engine, 0);
        let second = nth_position(&engine, 1);
        assert_ne!(first, second, "same-time anchors must differ");
        assert_eq!(
            engine.resolve_position(&Filter::default(), &second),
            ResolvePosition::Found(1),
        );
        assert_eq!(
            engine.resolve_position(&Filter::default(), &first),
            ResolvePosition::Found(0),
        );
        dir.cleanup();
    }

    #[test]
    fn cursor_at_end_positions_past_eof_for_every_source() {
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        append_bunyan_at(&a, "x", t(10), "a1");
        append_bunyan_at(&b, "x", t(20), "b1");
        let len_a = std::fs::metadata(&a).unwrap().len();
        let len_b = std::fs::metadata(&b).unwrap().len();

        let mut engine = Engine::new();
        let id_a = engine.add_file_source(&a).unwrap();
        let id_b = engine.add_file_source(&b).unwrap();
        let cursor = engine.cursor_at_end().unwrap();
        assert_eq!(cursor.get(&id_a), Some(ByteOffset::from(len_a)));
        assert_eq!(cursor.get(&id_b), Some(ByteOffset::from(len_b)));

        // A stepper at this cursor walks backward through the events
        // in reverse time order and returns nothing on forward.
        let mut stepper = engine.stepper(Filter::default(), &cursor);
        assert!(stepper.step_forward().is_none());
        let r1 = stepper.step_backward().unwrap();
        assert_eq!(r1.event.unwrap().msg, "b1");
        let r2 = stepper.step_backward().unwrap();
        assert_eq!(r2.event.unwrap().msg, "a1");
        assert!(stepper.step_backward().is_none());
        dir.cleanup();
    }

    #[test]
    fn cursor_for_position_lands_just_before_anchor() {
        // Build a multi-source fixture, anchor on a specific event,
        // and confirm step_forward from the resulting cursor returns
        // the anchored event itself.
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        append_bunyan_at(&a, "x", t(10), "a1");
        append_bunyan_at(&a, "x", t(30), "a2");
        append_bunyan_at(&b, "x", t(20), "b1");
        append_bunyan_at(&b, "x", t(40), "b2");

        let mut engine = Engine::new();
        engine.add_file_source(&a).unwrap();
        engine.add_file_source(&b).unwrap();
        // Merge order: a1(10), b1(20), a2(30), b2(40).  Anchor on b1.
        let anchor = nth_position(&engine, 1);
        let cursor = engine.cursor_for_position(&anchor).unwrap();
        // step_forward from this cursor returns b1; step_backward
        // returns the previous event a1.
        let mut stepper = engine.stepper(Filter::default(), &cursor);
        let fwd = stepper.step_forward().unwrap();
        assert_eq!(fwd.event.unwrap().msg, "b1");
        let mut stepper = engine.stepper(Filter::default(), &cursor);
        let back = stepper.step_backward().unwrap();
        assert_eq!(back.event.unwrap().msg, "a1");
        dir.cleanup();
    }

    #[test]
    fn cursor_for_position_distinguishes_same_time_ordinals() {
        // Two same-time events from one source.  cursor_for_position
        // should land just before the requested ordinal, not the first.
        let dir = TestDir::new();
        let p = dir.path().join("c.log");
        append_bunyan_at(&p, "x", t(10), "a");
        append_bunyan_at(&p, "x", t(10), "b");
        append_bunyan_at(&p, "x", t(20), "c");
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let pos_b = nth_position(&engine, 1);
        let cursor = engine.cursor_for_position(&pos_b).unwrap();
        let mut stepper = engine.stepper(Filter::default(), &cursor);
        // Forward: b (the anchored same-time second event).
        assert_eq!(stepper.step_forward().unwrap().event.unwrap().msg, "b",);
        let mut stepper = engine.stepper(Filter::default(), &cursor);
        // Backward: a (the same-time first event preceding b).
        assert_eq!(stepper.step_backward().unwrap().event.unwrap().msg, "a",);
        dir.cleanup();
    }

    #[test]
    fn cursor_for_position_returns_none_for_unattached_source() {
        // The bookmark's source isn't attached to this engine.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan_at(&p, "x", t(10), "a");
        let mut engine_a = Engine::new();
        engine_a.add_file_source(&p).unwrap();
        let anchor = nth_position(&engine_a, 0);
        let engine_b = Engine::new();
        assert!(engine_b.cursor_for_position(&anchor).is_none());
        dir.cleanup();
    }
}
