// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Engine: the library-level entry point used by both `seeit` and `seer`.
//!
//! The engine owns the set of [`Source`]s currently in play and hands
//! them out to a [`Stepper`] for time-merged iteration.  Each source
//! is assumed to be locally sorted by [`Event::time`]; the engine
//! does not re-check that invariant.

use crate::event::Event;
use crate::filter::Filter;
use crate::position::{ByteLen, LogStreamPosition, SourceId};
use crate::source::{FileSource, Source};
use camino::Utf8Path;
use std::sync::Arc;

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
    sources: Vec<Arc<dyn Source>>,
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
        self.sources.push(Arc::new(source));
        Ok(id)
    }

    /// Constructs a [`Stepper`] over every attached source whose id is
    /// accepted by `filter`'s source-id predicates, with each source's
    /// initial byte offset taken from `cursor` (defaulting to
    /// [`crate::source::ByteOffset::ZERO`] for sources missing from the
    /// cursor).
    ///
    /// The stepper fetches lazily and keeps per-source
    /// lookahead/lookbehind buffers so the TUI can scroll forward and
    /// backward without re-reading the whole file each time.  Filter
    /// changes are applied via [`Stepper::set_filter`] (which retains
    /// positions); changing the *source-id* filter requires building a
    /// fresh stepper, since the source set is fixed at construction.
    pub fn stepper(&self, filter: Filter, cursor: &Cursor) -> Stepper {
        self.stepper_with(filter, cursor, StepperOptions::default())
    }

    // XXX-dap TODO-doc
    pub fn stepper_batched(&self, filter: Filter, cursor: &Cursor) -> Stepper {
        self.stepper_with(
            filter,
            cursor,
            StepperOptions {
                max_records_to_scan_per_fill: Some(256), // XXX-dap
                ..Default::default()
            },
        )
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
    ) -> Stepper {
        let sources: Vec<Arc<dyn Source>> = self
            .sources
            .iter()
            .filter(|s| filter.matches_source_id(s.id()))
            .map(Arc::clone)
            .collect();
        Stepper::with_options(sources, filter, cursor, options)
    }

    /// Sum of `byte_len()` over every source whose id is accepted by
    /// `filter`'s source-id predicates.  Used as the denominator for a
    /// progress bar over a full-file pass: a long-running operation
    /// (Summary build, search across an unmatched file) can divide its
    /// running [`Stepper::walked_bytes`] by this to drive a percentage.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::ByteOffset;
    use crate::source::SourceError;
    use crate::test_fixtures::{TestDir, append_bunyan, append_bunyan_at, t};
    use slog::{error, info};

    /// Drains `engine.stepper(filter, &Cursor::new())` forward and
    /// returns the `Ok` event messages.  Used by the source-id-filter
    /// regression tests below: they care about which events make it
    /// past Engine-level source-id pruning, not about the stepper's
    /// own buffering details.
    fn forward_msgs(engine: &Engine, filter: &Filter) -> Vec<String> {
        let mut stepper = engine.stepper(filter.clone(), &Cursor::new());
        let mut out = Vec::new();
        while let Some(r) = stepper.step_forward() {
            if let Ok(event) = r.event() {
                out.push(event.msg.clone());
            }
        }
        out
    }

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
    fn out_of_order_display_includes_source_id() {
        // No production code path emits `SourceError::OutOfOrder`
        // today; the variant is reserved for a future regression
        // detector and its `Display` is what that detector would
        // surface to the user, so we keep the format pinned here.
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
    fn stepper_filters_by_source_id_regex() {
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
        assert_eq!(forward_msgs(&engine, &filter), vec!["n1", "n2"]);

        let filter: Filter = "source_id!~nexus".parse().unwrap();
        assert_eq!(forward_msgs(&engine, &filter), vec!["s1"]);

        dir.cleanup();
    }

    #[test]
    fn source_id_filter_skips_sources_without_iterating() {
        // A source whose path can't be opened (e.g. it was deleted
        // between `add_file_source` and the query) would normally
        // surface an `Io` error from its iterator.  When the source-id
        // filter rejects that source, the engine must not even
        // construct a per-source window for it — proven here by
        // deleting the file after registering it: an `Io` error would
        // only appear if the engine still tried to read it.
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
        // source; the regex must keep the engine from ever opening it.
        std::fs::remove_file(&agent).unwrap();

        let filter: Filter = "source_id=~nexus".parse().unwrap();
        let mut stepper = engine.stepper(filter, &Cursor::new());
        let mut records = Vec::new();
        while let Some(r) = stepper.step_forward() {
            records.push(r);
        }
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event().as_ref().unwrap().msg, "n1");

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
        assert_eq!(forward_msgs(&engine, &filter), vec!["n-error"]);

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
        assert_eq!(r1.event().as_ref().unwrap().msg, "b1");
        let r2 = stepper.step_backward().unwrap();
        assert_eq!(r2.event().as_ref().unwrap().msg, "a1");
        assert!(stepper.step_backward().is_none());
        dir.cleanup();
    }
}
