// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Engine: the library-level entry point used by both `seeit` and `seer`.
//!
//! The engine owns the set of [`Source`]s currently in play and exposes
//! queries over them.  Today the only query is "give me every event from
//! every source"; filters, merging, and lazy access will land here as the
//! design develops.

use crate::event::Event;
use crate::source::{FileSource, Source, SourceError, SourceId};
use camino::Utf8Path;

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

    /// Returns an iterator over every event in every source.
    ///
    /// Sources are consumed in the order they were added; events from
    /// each source are yielded contiguously.  Per-line parse and I/O
    /// errors appear inline as `Err` items so the stream isn't aborted
    /// by a single bad line.
    pub fn query_events(
        &self,
    ) -> impl Iterator<Item = Result<Event, SourceError>> + '_ {
        self.sources.iter().flat_map(|s| s.events())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Level;
    use crate::test_util::{TestDir, append_bunyan};
    use slog::{debug, error, info};

    #[test]
    fn query_concatenates_sources_in_add_order() {
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        append_bunyan(&a, "x", |log| {
            info!(log, "a1");
            info!(log, "a2");
        });
        append_bunyan(&b, "x", |log| {
            error!(log, "b1");
        });

        let mut engine = Engine::new();
        let id_a = engine.add_file_source(&a).unwrap();
        let id_b = engine.add_file_source(&b).unwrap();
        assert_ne!(id_a, id_b);

        let msgs: Vec<_> = engine
            .query_events()
            .map(|e| e.unwrap().msg)
            .collect();
        assert_eq!(msgs, vec!["a1", "a2", "b1"]);

        dir.cleanup();
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
        let levels: Vec<_> = engine
            .query_events()
            .map(|e| e.unwrap().level)
            .collect();
        assert_eq!(levels, vec![Level::Debug, Level::Error]);

        dir.cleanup();
    }
}
