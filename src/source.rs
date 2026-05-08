// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sources of log events.
//!
//! A [`Source`] is anything that can produce a sequence of [`Event`]s.
//! Today the only implementation is [`FileSource`], which reads bunyan
//! JSON one line at a time from a file on disk.  Future implementations
//! could include archives, network streams, or in-memory test fixtures.
//!
//! Each source also carries a [`SourceMetadata`], populated when the
//! source is opened, which captures the timestamps of the first and
//! last records and selected fields from the first record.  The engine
//! uses this for whole-source query pruning so a filter like
//! `name=Nexus` or a time-range bound can skip whole files without
//! ever opening them for reading.

use crate::event::{Event, Hostname, LoggerName};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, Utc};
use derive_more::{AsRef, Display, From};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
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

    /// Returns this source's metadata.
    ///
    /// The metadata is computed when the source is constructed and is
    /// intended for whole-source query pruning.  Implementations that
    /// cannot inexpensively compute metadata may return a default
    /// (all-`None`) value.
    fn metadata(&self) -> &SourceMetadata;

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

/// Coarse-grained facts about a source's content, derived from its
/// first and last records when the source is opened.
///
/// Used by the engine to skip whole sources at query time without
/// opening them for line-by-line reading: a query whose `name`
/// requirement disagrees with this source's `name`, or whose time
/// range does not overlap `[earliest, latest]`, can return zero
/// records immediately.  Each field is independent — first-record
/// failures leave `earliest`, `name`, and `hostname` as `None`;
/// last-record failures leave `latest` as `None` — so a partial
/// metadata is still useful.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceMetadata {
    /// time of the first record in the source, if a first record was
    /// present and parsed
    pub earliest: Option<DateTime<Utc>>,
    /// time of the last record in the source, if a last record was
    /// present and parsed
    pub latest: Option<DateTime<Utc>>,
    /// `name` field of the first record, if present
    pub name: Option<LoggerName>,
    /// `hostname` field of the first record, if present
    pub hostname: Option<Hostname>,
}

/// Source backed by a single file on disk.
///
/// Each call to [`Source::events`] re-opens the file and streams it line
/// by line, parsing each line as a bunyan JSON record.
pub struct FileSource {
    id: SourceId,
    path: Utf8PathBuf,
    metadata: SourceMetadata,
}

impl FileSource {
    /// Opens a file source at `path`.
    ///
    /// The path is canonicalized at construction; the canonical path
    /// becomes the source's [`SourceId`] and is used for subsequent
    /// reads.  The first and last records are also read at this time
    /// to populate [`SourceMetadata`]; failures to parse them are
    /// non-fatal and leave the corresponding fields `None`, but a
    /// hard I/O error from opening or seeking the file is propagated.
    pub fn open(path: &Utf8Path) -> std::io::Result<Self> {
        let canonical = path.canonicalize_utf8()?;
        let id = SourceId::from(canonical.as_str().to_string());
        let metadata = probe_metadata(&canonical)?;
        Ok(Self { id, path: canonical, metadata })
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

    fn metadata(&self) -> &SourceMetadata {
        &self.metadata
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

/// Reads the first and last records of `path` and uses them to fill in
/// a [`SourceMetadata`].
///
/// Hard I/O failures (opening, seeking, reading) are propagated.
/// Anything softer — a missing or unparseable first or last record —
/// is non-fatal and leaves the affected metadata fields `None`.  The
/// two probes are independent: a first-line parse failure does not
/// stop us from trying the last line, and vice versa.
fn probe_metadata(path: &Utf8Path) -> std::io::Result<SourceMetadata> {
    let mut file = File::open(path)?;
    let len = file.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(SourceMetadata::default());
    }

    file.seek(SeekFrom::Start(0))?;
    let first = read_first_line(&mut file)?;
    let first_event: Option<Event> =
        first.as_deref().and_then(|s| serde_json::from_str(s).ok());

    let last = read_last_line(&mut file, len)?;
    let last_event: Option<Event> =
        last.as_deref().and_then(|s| serde_json::from_str(s).ok());

    // Move fields out of `first_event` rather than cloning: the parsed
    // record is not needed beyond this assembly step.
    let (earliest, name, hostname) = match first_event {
        Some(e) => (Some(e.time), Some(e.name), Some(e.hostname)),
        None => (None, None, None),
    };
    let latest = last_event.map(|e| e.time);

    Ok(SourceMetadata { earliest, latest, name, hostname })
}

/// Reads the first line of `file` (up to and including the first
/// `\n`), returning the line without its terminator.  Returns
/// `Ok(None)` only when the file is empty.
fn read_first_line(file: &mut File) -> std::io::Result<Option<String>> {
    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    let n = reader.read_line(&mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(buf.trim_end_matches(['\r', '\n']).to_string()))
}

/// Reads the last line of `file`, where `len` is the file's length in
/// bytes.
///
/// A single trailing `\n` is treated as a record terminator and is
/// excluded from the returned line; a file that consists only of one
/// `\n` therefore has no last line and yields `Ok(None)`.  When no
/// preceding `\n` is found, the whole file (minus any trailing
/// newline) is returned as a single line.
///
/// `len` is taken as a parameter so the caller can avoid a redundant
/// `metadata()` syscall — [`probe_metadata`] already learned the
/// length by seeking to the end.
fn read_last_line(
    file: &mut File,
    len: u64,
) -> std::io::Result<Option<String>> {
    debug_assert!(len > 0);

    // Locate `end`: the byte index just past the last byte that
    // belongs to the last line.  Bunyan files conventionally end in
    // `\n`; treat that final newline as a terminator rather than as a
    // line of its own.
    file.seek(SeekFrom::Start(len - 1))?;
    let mut tail = [0u8; 1];
    file.read_exact(&mut tail)?;
    let end = if tail[0] == b'\n' { len - 1 } else { len };
    if end == 0 {
        return Ok(None);
    }

    // Scan backwards in chunks for the previous `\n`.  Everything
    // strictly between that `\n` and `end` is the last line.  When no
    // preceding `\n` is found anywhere in the file, the line spans
    // from byte 0.
    const CHUNK: u64 = 4096;
    let mut search_end = end;
    let line_start: u64 = loop {
        let chunk_size = std::cmp::min(CHUNK, search_end);
        let chunk_start = search_end - chunk_size;
        file.seek(SeekFrom::Start(chunk_start))?;
        // `chunk_size` is bounded above by `CHUNK`, so this `as usize`
        // cannot truncate on any supported target.
        let mut buf = vec![0u8; chunk_size as usize];
        file.read_exact(&mut buf)?;
        if let Some(idx) = buf.iter().rposition(|&b| b == b'\n') {
            break chunk_start + (idx as u64) + 1;
        }
        if chunk_start == 0 {
            break 0;
        }
        search_end = chunk_start;
    };

    let line_len = usize::try_from(end - line_start).map_err(|_| {
        std::io::Error::other("last line too large to read into memory")
    })?;
    file.seek(SeekFrom::Start(line_start))?;
    let mut buf = vec![0u8; line_len];
    file.read_exact(&mut buf)?;
    let s = String::from_utf8(buf).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    Ok(Some(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{
        TestDir, append_bunyan, append_bunyan_at, append_raw,
    };
    use chrono::TimeZone;
    use slog::{error, info};

    /// Builds a [`DateTime<Utc>`] from epoch seconds.  Mirrors the
    /// helper in the engine tests; metadata tests anchor on specific
    /// timestamps to assert exact `earliest` / `latest` values.
    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid timestamp")
    }

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

    #[test]
    fn metadata_empty_file_is_default() {
        // An empty file has no first or last record; every metadata
        // field should be `None`, but `open` must still succeed.
        let dir = TestDir::new();
        let p = dir.path().join("empty.log");
        std::fs::File::create(&p).unwrap();
        let src = FileSource::open(&p).unwrap();
        assert_eq!(src.metadata(), &SourceMetadata::default());
        dir.cleanup();
    }

    #[test]
    fn metadata_single_record_earliest_equals_latest() {
        let dir = TestDir::new();
        let p = dir.path().join("one.log");
        append_bunyan_at(&p, "Nexus", t(100), "hello");
        let src = FileSource::open(&p).unwrap();
        let m = src.metadata();
        assert_eq!(m.earliest, Some(t(100)));
        assert_eq!(m.latest, Some(t(100)));
        assert_eq!(m.name.as_ref().map(|n| n.to_string()), Some("Nexus".to_string()));
        assert_eq!(
            m.hostname.as_ref().map(|h| h.to_string()),
            Some("test-host".to_string()),
        );
        dir.cleanup();
    }

    #[test]
    fn metadata_multi_record_uses_first_and_last_times() {
        let dir = TestDir::new();
        let p = dir.path().join("multi.log");
        append_bunyan_at(&p, "Nexus", t(10), "a");
        append_bunyan_at(&p, "Nexus", t(20), "b");
        append_bunyan_at(&p, "Nexus", t(30), "c");
        let src = FileSource::open(&p).unwrap();
        let m = src.metadata();
        assert_eq!(m.earliest, Some(t(10)));
        assert_eq!(m.latest, Some(t(30)));
        assert_eq!(m.name.as_ref().map(|n| n.to_string()), Some("Nexus".to_string()));
        dir.cleanup();
    }

    #[test]
    fn metadata_handles_missing_trailing_newline() {
        // `append_bunyan_at` always writes a trailing newline; build the
        // file by hand to omit it on the last line.
        let dir = TestDir::new();
        let p = dir.path().join("no_trailing_nl.log");
        let first = serde_json::json!({
            "v": 0, "level": 30, "name": "Nexus", "hostname": "h",
            "pid": 1, "time": t(10).to_rfc3339(), "msg": "first",
        })
        .to_string();
        let last = serde_json::json!({
            "v": 0, "level": 30, "name": "Nexus", "hostname": "h",
            "pid": 1, "time": t(20).to_rfc3339(), "msg": "last",
        })
        .to_string();
        std::fs::write(&p, format!("{first}\n{last}")).unwrap();
        let src = FileSource::open(&p).unwrap();
        let m = src.metadata();
        assert_eq!(m.earliest, Some(t(10)));
        assert_eq!(m.latest, Some(t(20)));
        dir.cleanup();
    }

    #[test]
    fn metadata_unparseable_first_line_leaves_first_fields_none() {
        // First line is non-JSON noise (e.g. SMF preamble); last line
        // is a real bunyan record.  We should still capture `latest`
        // even though `earliest`/`name`/`hostname` are unknown.
        let dir = TestDir::new();
        let p = dir.path().join("bad_first.log");
        append_raw(&p, "[ Mar 14 12:00:00 svc.startd starting log ]");
        append_bunyan_at(&p, "Nexus", t(20), "real");
        let src = FileSource::open(&p).unwrap();
        let m = src.metadata();
        assert_eq!(m.earliest, None);
        assert_eq!(m.latest, Some(t(20)));
        assert_eq!(m.name, None);
        assert_eq!(m.hostname, None);
        dir.cleanup();
    }

    #[test]
    fn metadata_unparseable_last_line_leaves_latest_none() {
        // First line parses; last line is a tail-of-log SMF banner.
        // `earliest`/`name`/`hostname` should be populated; `latest`
        // should be `None`.
        let dir = TestDir::new();
        let p = dir.path().join("bad_last.log");
        append_bunyan_at(&p, "Nexus", t(10), "real");
        append_raw(&p, "[ Mar 14 13:00:00 svc.startd shutting down ]");
        let src = FileSource::open(&p).unwrap();
        let m = src.metadata();
        assert_eq!(m.earliest, Some(t(10)));
        assert_eq!(m.latest, None);
        assert_eq!(
            m.name.as_ref().map(|n| n.to_string()),
            Some("Nexus".to_string()),
        );
        dir.cleanup();
    }

    #[test]
    fn metadata_only_newlines_yields_default() {
        // A file consisting entirely of newlines has no parseable
        // record at either end; `latest` is unset because the byte
        // before the trailing newline is itself a newline (so the
        // last "line" is empty), and `earliest` is unset because
        // serde rejects the empty string.
        let dir = TestDir::new();
        let p = dir.path().join("newlines.log");
        std::fs::write(&p, "\n\n\n").unwrap();
        let src = FileSource::open(&p).unwrap();
        assert_eq!(src.metadata(), &SourceMetadata::default());
        dir.cleanup();
    }

    #[test]
    fn metadata_last_line_across_chunk_boundary() {
        // The backward chunked read in `read_last_line` walks the file
        // in 4 KiB chunks.  Make the last record larger than that so
        // the previous newline lives in an earlier chunk; this catches
        // off-by-one errors at the chunk seam.
        let dir = TestDir::new();
        let p = dir.path().join("big_last.log");
        append_bunyan_at(&p, "Nexus", t(10), "first");
        let big = "x".repeat(10_000);
        append_bunyan_at(&p, "Nexus", t(20), &big);
        let src = FileSource::open(&p).unwrap();
        let m = src.metadata();
        assert_eq!(m.earliest, Some(t(10)));
        assert_eq!(m.latest, Some(t(20)));
        dir.cleanup();
    }

    #[test]
    fn metadata_single_long_line_no_newlines() {
        // The file has no `\n` anywhere; the entire file is one line.
        // Both probes should resolve to that single record.
        let dir = TestDir::new();
        let p = dir.path().join("one_long.log");
        let line = serde_json::json!({
            "v": 0, "level": 30, "name": "Nexus", "hostname": "h",
            "pid": 1, "time": t(42).to_rfc3339(),
            "msg": "x".repeat(8_000),
        })
        .to_string();
        std::fs::write(&p, &line).unwrap();
        let src = FileSource::open(&p).unwrap();
        let m = src.metadata();
        assert_eq!(m.earliest, Some(t(42)));
        assert_eq!(m.latest, Some(t(42)));
        dir.cleanup();
    }

    #[test]
    fn metadata_uses_first_records_name_and_hostname_not_lasts() {
        // When the first record's `name` differs from the last
        // record's, metadata reflects the first.  This matches the
        // documented contract — the engine prunes by what the file
        // *starts* with, since most bunyan log files in practice keep
        // a single component throughout.
        let dir = TestDir::new();
        let p = dir.path().join("mixed_names.log");
        let first = serde_json::json!({
            "v": 0, "level": 30, "name": "Nexus", "hostname": "h-first",
            "pid": 1, "time": t(10).to_rfc3339(), "msg": "a",
        })
        .to_string();
        let last = serde_json::json!({
            "v": 0, "level": 30, "name": "SledAgent", "hostname": "h-last",
            "pid": 1, "time": t(20).to_rfc3339(), "msg": "b",
        })
        .to_string();
        std::fs::write(&p, format!("{first}\n{last}\n")).unwrap();
        let src = FileSource::open(&p).unwrap();
        let m = src.metadata();
        assert_eq!(
            m.name.as_ref().map(|n| n.to_string()),
            Some("Nexus".to_string()),
        );
        assert_eq!(
            m.hostname.as_ref().map(|h| h.to_string()),
            Some("h-first".to_string()),
        );
        dir.cleanup();
    }
}
