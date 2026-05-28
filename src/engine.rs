// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Engine: the library-level entry point used by both `seeit` and `seer`.
//!
//! The engine owns the set of [`Source`]s currently in play and exposes
//! queries over them.  Multi-source queries are time-merged: each source
//! is assumed to be locally sorted by [`Event::time`] (when it is not,
//! the engine emits a one-shot [`SourceError::OutOfOrder`] warning for
//! that source and proceeds anyway).

use crate::event::Event;
use crate::filter::Filter;
use crate::position::{ByteLen, LogStreamPosition, SourceId};
use crate::source::{FileSource, Source, SourceError};
use camino::Utf8Path;
use chrono::{DateTime, Utc};
use std::collections::VecDeque;

mod merge;

pub use crate::position::Cursor;
pub use merge::{
    FETCH_BATCH_SIZE, MergeError, MergeRecord, Stepper, StepperOptions,
};

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
        self.stepper_with(filter, cursor, StepperOptions::default())
    }

    /// Like [`Self::stepper`] but lets the caller customize the
    /// per-fill batch size and an optional per-fill records-walked
    /// budget via [`StepperOptions`].  The TUI's long-op driver uses
    /// this to keep each tick responsive under selective filters
    /// (one match per call, paired with a `max_records_to_scan_per_fill`
    /// budget — see [`crate::StreamView::ensure_window_step`]).
    /// Without the budget, a single fill under a selective filter can
    /// freeze the UI for hundreds of milliseconds at a time.
    pub fn stepper_with(
        &self,
        filter: Filter,
        cursor: &Cursor,
        options: StepperOptions,
    ) -> Stepper<'_> {
        let sources: Vec<&dyn Source> = self
            .sources
            .iter()
            .filter(|s| filter.matches_source_id(s.id()))
            .map(|s| s.as_ref())
            .collect();
        Stepper::with_options(sources, filter, cursor, options)
    }

    /// Sum of `byte_len()` over every source whose id is accepted by
    /// `filter`'s source-id predicates.  Used as the denominator for a
    /// progress bar over a full-file pass: a long-running operation
    /// (Summary build, search across an unmatched file) can divide its
    /// running [`EventStream::bytes_read`] by this to drive a percentage.
    /// Sources whose `byte_len()` syscall fails contribute zero — the
    /// progress will overshoot rather than fail outright, which is the
    /// less surprising behavior here.
    pub fn filtered_total_bytes(&self, filter: &Filter) -> ByteLen {
        self.sources
            .iter()
            .filter(|s| filter.matches_source_id(s.id()))
            .map(|s| ByteLen::from(s.byte_len().unwrap_or(0)))
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
    bytes_read: ByteLen,
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
            bytes_read: ByteLen::ZERO,
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
        self.bytes_read += ByteLen::from(bytes);
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
    pub fn bytes_read(&self) -> ByteLen {
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
                && !self.filter.matches_event(&ee.event)
            {
                continue;
            }
            return Some(item);
        }
    }
}

/// Pops the next item across `cursors`: error/warning heads are emitted ahead
/// of any event head; among event heads, smallest timestamp wins; ties break by
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
    use crate::position::ByteOffset;
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
        assert_eq!(total, ByteLen::from(a_size + b_size));

        // A source-id predicate that excludes `b` drops `b`'s bytes
        // from the denominator; a real progress bar driven by this
        // value would track only the sources that will actually be
        // scanned.
        let path_filter: Filter = "source_id!~b\\.log".parse().unwrap();
        assert_eq!(
            engine.filtered_total_bytes(&path_filter),
            ByteLen::from(a_size)
        );

        dir.cleanup();
    }

    #[test]
    fn filtered_total_bytes_empty_engine_is_zero() {
        let engine = Engine::new();
        assert_eq!(
            engine.filtered_total_bytes(&Filter::default()),
            ByteLen::ZERO
        );
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
}
