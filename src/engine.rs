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
use crate::source::{FileSource, Source, SourceError, SourceId};
use camino::Utf8Path;
use chrono::{DateTime, Utc};
use std::collections::VecDeque;

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

    /// Returns an iterator over every event in every source that
    /// matches `filter`.
    ///
    /// Events from different sources are interleaved by time so the
    /// caller sees the full timeline, not one source at a time.  Each
    /// source is assumed to be locally sorted; if a source has an
    /// out-of-order entry, a one-shot [`SourceError::OutOfOrder`]
    /// warning for that source is emitted *before* the offending event
    /// (and the merge continues with the timestamps as given).  Per-line
    /// parse and I/O errors appear inline as `Err` items, kept close to
    /// their position in the source file rather than being slotted by
    /// time.  The filter only applies to `Ok` items, so errors and
    /// warnings are always surfaced regardless of the filter.
    pub fn query_events<'a>(
        &'a self,
        filter: &'a Filter,
    ) -> impl Iterator<Item = Result<Event, SourceError>> + 'a {
        let cursors = self
            .sources
            .iter()
            .map(|s| SourceCursor::new(s.as_ref()))
            .collect();
        MergeIter { cursors }.filter(move |r| match r {
            Ok(e) => filter.matches(e),
            Err(_) => true,
        })
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
    iter: Box<dyn Iterator<Item = Result<Event, SourceError>> + 'a>,
    pending: VecDeque<Result<Event, SourceError>>,
    last_time: Option<DateTime<Utc>>,
    source_id: SourceId,
    out_of_order_warned: bool,
}

impl<'a> SourceCursor<'a> {
    fn new(source: &'a dyn Source) -> Self {
        Self {
            iter: source.events(),
            pending: VecDeque::new(),
            last_time: None,
            source_id: source.id().clone(),
            out_of_order_warned: false,
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
        let Some(item) = self.iter.next() else {
            return;
        };
        if let Ok(ev) = &item
            && let Some(prev) = self.last_time
            && ev.time < prev
            && !self.out_of_order_warned
        {
            self.pending.push_back(Err(SourceError::OutOfOrder {
                source_id: self.source_id.clone(),
                seen: ev.time,
                last_seen: prev,
            }));
            self.out_of_order_warned = true;
        }
        if let Ok(ev) = &item {
            self.last_time = Some(ev.time);
        }
        self.pending.push_back(item);
    }

    /// Returns the head of the cursor without consuming it.  Returns
    /// `None` once the source is fully drained.
    fn peek(&mut self) -> Option<&Result<Event, SourceError>> {
        self.fill();
        self.pending.front()
    }

    /// Consumes and returns the head of the cursor.
    fn pop(&mut self) -> Option<Result<Event, SourceError>> {
        self.fill();
        self.pending.pop_front()
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
    type Item = Result<Event, SourceError>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut best: Option<usize> = None;
        let mut best_time: Option<DateTime<Utc>> = None;
        let mut best_is_err = false;

        for i in 0..self.cursors.len() {
            // Decide based on this cursor's head, then drop the borrow
            // so the next iteration can mutably borrow another cursor.
            let outcome = match self.cursors[i].peek() {
                None => None,
                Some(Err(_)) => Some((true, None)),
                Some(Ok(ev)) => Some((false, Some(ev.time))),
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
        self.cursors[idx].pop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Level;
    use crate::test_util::{
        TestDir, append_bunyan, append_bunyan_at, append_raw,
    };
    use chrono::TimeZone;
    use slog::{debug, error, info};

    /// Builds a [`DateTime<Utc>`] from epoch seconds.  Tests use this to
    /// pin event timestamps to predictable values so the merge order
    /// can be asserted exactly.
    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid timestamp")
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
            .map(|e| e.unwrap().msg)
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
            .map(|e| e.unwrap().msg)
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
            .map(|e| e.unwrap().msg)
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
        let results: Vec<_> =
            engine.query_events(&Filter::default()).collect();
        assert_eq!(results.len(), 4);
        assert_eq!(results[0].as_ref().unwrap().msg, "a1");
        assert!(matches!(
            results[1].as_ref().unwrap_err(),
            SourceError::Parse(_),
        ));
        assert_eq!(results[2].as_ref().unwrap().msg, "b1");
        assert_eq!(results[3].as_ref().unwrap().msg, "a2");

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
        let results: Vec<_> =
            engine.query_events(&Filter::default()).collect();

        // Output: first, second, [warning], third, fourth.  The warning
        // is emitted just before the first regressing event, and only
        // once for the source.
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].as_ref().unwrap().msg, "first");
        assert_eq!(results[1].as_ref().unwrap().msg, "second");
        let warning = results[2].as_ref().unwrap_err();
        match warning {
            SourceError::OutOfOrder { seen, last_seen, .. } => {
                assert_eq!(*seen, t(20));
                assert_eq!(*last_seen, t(30));
            }
            other => panic!("expected OutOfOrder, got {other:?}"),
        }
        assert_eq!(results[3].as_ref().unwrap().msg, "third");
        assert_eq!(results[4].as_ref().unwrap().msg, "fourth");

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
        let results: Vec<_> =
            engine.query_events(&Filter::default()).collect();
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
            .map(|e| e.unwrap().level)
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
            .map(|r| r.unwrap().msg)
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
            .map(|r| r.unwrap().msg)
            .collect();
        assert_eq!(msgs, vec!["blueprint executed", "blueprint failed"]);

        dir.cleanup();
    }
}
