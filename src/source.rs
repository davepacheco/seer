// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sources of log events.
//!
//! A [`Source`] is anything that can produce a sequence of [`Event`]s.
//! Today the only implementation is [`FileSource`], which reads bunyan
//! JSON one line at a time from a file on disk.  Future implementations
//! could include archives, network streams, or in-memory test fixtures.

use crate::event::Event;
use camino::{Utf8Path, Utf8PathBuf};
use derive_more::{AsRef, Display, From};
use std::fs::File;
use std::io::{BufRead, BufReader};

/// Identifier for a source.
///
/// Wraps a string so different `Source` impls can choose the most useful
/// shape for their identifier (canonicalized path, archive entry name,
/// URL, etc.) without forcing a single representation on the type.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Display, From, AsRef,
)]
#[as_ref(forward)]
pub struct SourceId(String);

/// Error produced while reading events from a source.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse log line as JSON: {0}")]
    Parse(#[from] serde_json::Error),
}

/// A source of log events.
pub trait Source {
    /// Returns this source's identifier.
    fn id(&self) -> &SourceId;

    /// Returns an iterator that yields each event in turn, with parse and
    /// I/O errors surfaced per item rather than aborting the stream.
    fn events<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = Result<Event, SourceError>> + 'a>;
}

/// Source backed by a single file on disk.
///
/// Each call to [`Source::events`] re-opens the file and streams it line
/// by line, parsing each line as a bunyan JSON record.
pub struct FileSource {
    id: SourceId,
    path: Utf8PathBuf,
}

impl FileSource {
    /// Opens a file source at `path`.
    ///
    /// The path is canonicalized at construction; the canonical path
    /// becomes the source's [`SourceId`] and is used for subsequent
    /// reads.
    pub fn open(path: &Utf8Path) -> std::io::Result<Self> {
        let canonical = path.canonicalize_utf8()?;
        let id = SourceId::from(canonical.as_str().to_string());
        Ok(Self { id, path: canonical })
    }

    /// Returns the canonicalized path this source reads from.
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }
}

impl Source for FileSource {
    fn id(&self) -> &SourceId {
        &self.id
    }

    fn events<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = Result<Event, SourceError>> + 'a> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) => {
                return Box::new(std::iter::once(Err(e.into())));
            }
        };
        let reader = BufReader::new(file);
        Box::new(reader.lines().map(|line| {
            let line = line?;
            let event: Event = serde_json::from_str(&line)?;
            Ok(event)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{TestDir, append_bunyan, append_raw};
    use slog::{error, info};

    #[test]
    fn file_source_id_is_canonical_path() {
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        // Create an empty file so canonicalize_utf8 can resolve it.
        std::fs::File::create(&p).unwrap();
        let src = FileSource::open(&p).unwrap();
        // canonicalize_utf8 resolves any symlinks/relative parts; on a
        // freshly-created tempdir the path should already be absolute,
        // so the id matches the path string we just opened.
        let id: &str = src.id().as_ref();
        assert_eq!(id, src.path().as_str());
        dir.cleanup();
    }

    #[test]
    fn file_source_streams_events_and_surfaces_parse_errors() {
        let dir = TestDir::new();
        let p = dir.path().join("b.log");
        append_bunyan(&p, "a", |log| {
            info!(log, "first");
        });
        append_raw(&p, "not json at all");
        append_bunyan(&p, "a", |log| {
            error!(log, "third");
        });

        let src = FileSource::open(&p).unwrap();
        let results: Vec<_> = src.events().collect();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap().msg, "first");
        assert!(matches!(
            results[1].as_ref().unwrap_err(),
            SourceError::Parse(_)
        ));
        assert_eq!(results[2].as_ref().unwrap().msg, "third");

        dir.cleanup();
    }
}
