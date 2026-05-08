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
use chrono::{DateTime, Utc};
use derive_more::{AsRef, Display, From};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::iter;

/// Identifier for a source.
///
/// Wraps a string so different `Source` impls can choose the most useful
/// shape for their identifier (canonicalized path, archive entry name,
/// URL, etc.) without forcing a single representation on the type.
///
/// Implements `Serialize`/`Deserialize` so it can ride inside a
/// [`crate::stream::LogStreamPosition`] in persisted session state.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Display,
    From,
    AsRef,
    serde::Serialize,
    serde::Deserialize,
)]
#[as_ref(forward)]
#[serde(transparent)]
pub struct SourceId(String);

/// A non-event item surfaced by a source — either a true error
/// encountered while reading (`Io`, `Parse`) or a non-fatal warning
/// about the source's content (`OutOfOrder`).  All variants ride the
/// same `Err` channel of the iterator returned by [`Source::events`]
/// and the engine's merge so callers can render them inline next to
/// real events; the variant's `Display` is responsible for spelling
/// out whether it is a warning or an error.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse log line as JSON: {0}")]
    Parse(#[from] serde_json::Error),
    /// Reported once per source by the engine when an event with a
    /// timestamp earlier than the previous event is observed.  Merging
    /// across sources assumes each source is itself sorted by time;
    /// when that assumption is violated the merge can no longer
    /// guarantee its output is sorted, so we surface a warning rather
    /// than failing.
    #[error(
        "warning: source {source_id} is not sorted by time: \
         {seen} appeared after {last_seen}"
    )]
    OutOfOrder {
        source_id: SourceId,
        seen: DateTime<Utc>,
        last_seen: DateTime<Utc>,
    },
}

/// A source of log events.
pub trait Source {
    /// Returns this source's identifier.
    fn id(&self) -> &SourceId;

    /// Returns an iterator that yields each event in turn, paired with
    /// the number of source bytes consumed for that record (including
    /// the trailing newline, when present).
    ///
    /// Bytes are reported for parse errors too, since the error
    /// originated from a line we read.  An I/O error returned for the
    /// very first item (when the file can't be opened, for instance)
    /// reports `0` bytes since nothing was read off disk.  The byte
    /// counts are surfaced so the engine can show users a parse-rate
    /// status line without re-walking the input.
    fn events<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = (u64, Result<Event, SourceError>)> + 'a>;
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
    ) -> Box<dyn Iterator<Item = (u64, Result<Event, SourceError>)> + 'a> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) => {
                return Box::new(iter::once((0, Err(e.into()))));
            }
        };
        // Drive the read with `read_line` rather than `lines()` so the
        // byte count includes the line terminator — that's what we
        // want to surface to the user as "bytes read off disk".
        let mut reader = BufReader::new(file);
        let mut buf = String::new();
        Box::new(iter::from_fn(move || {
            buf.clear();
            match reader.read_line(&mut buf) {
                Ok(0) => None,
                Ok(n) => {
                    let line = buf.trim_end_matches(['\r', '\n']);
                    let result = serde_json::from_str::<Event>(line)
                        .map_err(SourceError::from);
                    Some((n as u64, result))
                }
                Err(e) => Some((0, Err(e.into()))),
            }
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
        assert_eq!(results[0].1.as_ref().unwrap().msg, "first");
        assert!(matches!(
            results[1].1.as_ref().unwrap_err(),
            SourceError::Parse(_),
        ));
        assert_eq!(results[2].1.as_ref().unwrap().msg, "third");
        // Each line consumed at least its content's bytes plus a
        // trailing newline.  Parse-error lines are byte-counted too.
        for (bytes, _) in &results {
            assert!(*bytes > 0);
        }

        dir.cleanup();
    }

    #[test]
    fn file_source_byte_counts_sum_to_file_size() {
        // The engine sums per-record byte counts to drive the parse
        // status line; the sum had better equal what `wc -c` would say.
        let dir = TestDir::new();
        let p = dir.path().join("c.log");
        append_bunyan(&p, "a", |log| {
            info!(log, "first");
            info!(log, "second");
        });
        append_raw(&p, "not json");

        let src = FileSource::open(&p).unwrap();
        let total_bytes: u64 = src.events().map(|(b, _)| b).sum();
        let file_size = std::fs::metadata(&p).unwrap().len();
        assert_eq!(total_bytes, file_size);

        dir.cleanup();
    }
}
