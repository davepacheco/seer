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
use crate::filter::{Filter, Predicate};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, Utc};
use derive_more::{AsRef, Display, From};
use schemars::JsonSchema;
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
    JsonSchema,
)]
#[as_ref(forward)]
#[serde(transparent)]
pub struct SourceId(String);

/// Byte offset into a source's underlying bytes.
///
/// A newtype around `u64` so an offset can't be silently confused
/// with a length, count, or any other unsigned quantity that turns up
/// in adjacent code.  `Copy + Ord` so it can be used as a `BTreeMap`
/// key (the engine's eventual merged-stream cursor is a
/// `BTreeMap<SourceId, ByteOffset>`).
///
/// Convention: an offset always names the byte at which the *next*
/// record would start when scanning forward — equivalently, the byte
/// just past the end of the previous record.  Backward scans honor
/// the same convention: an offset of `N` reads the record whose end
/// is at `N`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Display,
    From,
    serde::Serialize,
    serde::Deserialize,
    JsonSchema,
)]
#[serde(transparent)]
pub struct ByteOffset(u64);

impl ByteOffset {
    /// Byte offset zero — the start of any source.
    pub const ZERO: Self = Self(0);

    /// Returns the offset as a raw `u64`.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Direction of a [`Source::query`] scan.
///
/// `Forward` reads records starting at the requested offset and
/// advancing toward EOF.  `Backward` reads the record whose end is at
/// the requested offset and walks toward BOF.  Using an enum (rather
/// than a `bool reverse` parameter) keeps call sites self-documenting
/// and makes the code unambiguous to read at a distance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

/// A single record returned by [`Source::query`].
///
/// The `offset` is where this record starts in the source's bytes;
/// `length` is its length in bytes including any trailing `\n`.  In
/// particular, `offset + length` is the offset at which the next
/// forward record begins (or, equivalently, the offset to pass back
/// to [`Source::query`] to read further forward).  A backward query
/// uses `offset` itself as the next call's offset.
///
/// `event` carries either the parsed [`Event`] or the
/// [`SourceError`] from a line that failed to parse — parse errors
/// are surfaced inline rather than aborting the query, since a single
/// bad line is rarely interesting enough to lose the rest of the
/// file's events to.
///
/// `raw` is the record's bytes from the source, minus any trailing
/// `\r`/`\n` terminator, decoded as UTF-8 (lossy on the rare invalid
/// byte sequence).  Carried alongside the parsed event so the TUI can
/// switch between formatted and raw rendering without re-reading the
/// file.  Memory cost is bounded by the caller's window size, since
/// `query` only returns up to `count` records per call.
#[derive(Debug)]
pub struct QueryRecord {
    pub offset: ByteOffset,
    pub length: u64,
    pub event: Result<Event, SourceError>,
    pub raw: String,
}

/// One scan's worth of records plus accounting that lets the caller
/// drive a progress bar and resume cleanly when a walks budget is in
/// effect.
///
/// `walked_bytes` is the total bytes the scan consumed off disk —
/// including records that were rejected by the filter and never made
/// it into `records`.  Lets the TUI's long-op driver show meaningful
/// percent-done feedback even during sparse filter regions, where
/// many on-disk records get walked without any landing in the
/// streamview.
///
/// `eof` is true when the scan ran out of records in the chosen
/// direction (forward: hit EOF; backward: walked to byte 0).  When
/// it's false but `records.len() < count`, the scan stopped because
/// the caller's `max_walks` budget was reached; the caller can resume
/// by issuing a fresh query from `offset + walked_bytes` (forward) or
/// `offset - walked_bytes` (backward).
#[derive(Debug)]
pub struct QueryBatch {
    pub records: Vec<QueryRecord>,
    pub walked_bytes: u64,
    pub eof: bool,
}

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

    /// Reads up to `count` records from `offset` in `direction` whose
    /// events are accepted by `filter`.
    ///
    /// `offset` follows the [`ByteOffset`] convention: forward,
    /// records are read starting at the byte; backward, the record
    /// whose end is at the byte is read first.  Records whose events
    /// fail the filter are silently skipped — the scan continues
    /// until `count` matching records have been collected or the end
    /// of the source (in the chosen direction) is reached.  Lines
    /// that fail to parse as JSON are surfaced as inline `Err` items
    /// in the returned vector and *do* count toward `count`, since a
    /// run of malformed lines would otherwise scan the entire file
    /// looking for matches.
    ///
    /// Implementations are expected to consult [`Self::metadata`] for
    /// whole-source pruning before doing any I/O — see
    /// [`SourceMetadata::excludes_all`].
    ///
    /// I/O errors that prevent the source from being read at all
    /// (file gone, permission denied) are returned as `Err`; per-line
    /// parse errors do not.
    fn query(
        &self,
        offset: ByteOffset,
        direction: Direction,
        count: usize,
        filter: &Filter,
    ) -> std::io::Result<Vec<QueryRecord>> {
        self.query_bounded(offset, direction, count, None, filter)
            .map(|b| b.records)
    }

    /// Like [`Self::query`] but with a per-call records-walked budget
    /// and an explicit "ran out of source" indicator.
    ///
    /// `max_walks` caps how many records (matching or not) the scan
    /// will examine on disk before returning.  When the budget is hit
    /// before either `count` matches or the source edge is reached,
    /// the returned [`QueryBatch::eof`] is `false` and `records.len()`
    /// may be under `count`; the caller can resume from
    /// `offset ± walked_bytes` (forward / backward) to continue.
    ///
    /// Used by the TUI's long-op driver (g/G/filter rebuild) so each
    /// tick walks only a bounded number of records and the UI gets a
    /// chance to render the progress bar between ticks.  Passing
    /// `None` for `max_walks` is equivalent to [`Self::query`].
    fn query_bounded(
        &self,
        offset: ByteOffset,
        direction: Direction,
        count: usize,
        max_walks: Option<usize>,
        filter: &Filter,
    ) -> std::io::Result<QueryBatch>;

    /// Returns this source's current size in bytes.
    ///
    /// Used by callers that want to position a [`ByteOffset`] cursor
    /// past the last record (e.g. for "jump to end" navigation).  The
    /// engine's `Stepper` accepts a cursor whose offsets exceed the
    /// file size — backward queries clamp internally — so this is
    /// purely a convenience for synthesizing such a cursor.
    fn byte_len(&self) -> std::io::Result<u64>;
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

impl SourceMetadata {
    /// Returns `true` iff this source provably contains no events
    /// matching `filter`, given only the metadata.
    ///
    /// Today this checks `name` and `hostname` equality predicates
    /// (`name=Nexus`, `hostname!=foo`, etc.) against the recorded
    /// first-record values.  When the metadata's value disagrees with
    /// what such a predicate would accept, the predicate is taken to
    /// reject every event in the source and the source is excluded
    /// without ever being read.
    ///
    /// This is heuristic.  It assumes that `name` and `hostname` are
    /// uniform within a source, which is true of every Oxide bunyan
    /// log file in practice — each file is one component on one
    /// host — but a hand-mixed file would defeat the heuristic.  The
    /// cost of being wrong is missed records, not incorrect output
    /// from records that *are* returned.  Predicates we can't
    /// evaluate at this layer (`level`, `msg=~`, extra-field
    /// matchers, etc.) are simply ignored here and applied during
    /// the per-record scan.
    pub fn excludes_all(&self, filter: &Filter) -> bool {
        for predicate in filter.predicates() {
            let Predicate::FieldEquals { name, value, negated } = predicate
            else {
                continue;
            };
            let known_value: &str = match name.as_str() {
                "name" => match &self.name {
                    Some(n) => n.as_ref(),
                    None => continue,
                },
                "hostname" => match &self.hostname {
                    Some(h) => h.as_ref(),
                    None => continue,
                },
                _ => continue,
            };
            let predicate_passes = (known_value == value) ^ *negated;
            if !predicate_passes {
                return true;
            }
        }
        false
    }
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

    fn byte_len(&self) -> std::io::Result<u64> {
        Ok(std::fs::metadata(&self.path)?.len())
    }

    fn query_bounded(
        &self,
        offset: ByteOffset,
        direction: Direction,
        count: usize,
        max_walks: Option<usize>,
        filter: &Filter,
    ) -> std::io::Result<QueryBatch> {
        if count == 0 || self.metadata.excludes_all(filter) {
            return Ok(QueryBatch {
                records: Vec::new(),
                walked_bytes: 0,
                eof: true,
            });
        }
        let mut file = File::open(&self.path)?;
        let len = file.seek(SeekFrom::End(0))?;
        let mut results = Vec::with_capacity(count);
        let walked_bytes;
        let eof;
        match direction {
            Direction::Forward => {
                if offset.get() < len {
                    let (walked, hit_end) = scan_forward(
                        &mut file,
                        offset.get(),
                        count,
                        max_walks,
                        filter,
                        &mut results,
                    )?;
                    walked_bytes = walked;
                    eof = hit_end;
                } else {
                    walked_bytes = 0;
                    eof = true;
                }
            }
            Direction::Backward => {
                // A stale cursor past EOF clamps to EOF; the caller
                // gets the last `count` records as if they had asked
                // from EOF in the first place.
                let bounded = std::cmp::min(offset.get(), len);
                let (walked, hit_end) = scan_backward(
                    &mut file,
                    bounded,
                    count,
                    max_walks,
                    filter,
                    &mut results,
                )?;
                walked_bytes = walked;
                eof = hit_end;
            }
        }
        Ok(QueryBatch { records: results, walked_bytes, eof })
    }
}

/// Walks `file` forward from `start_offset`, parsing one line at a
/// time and pushing accepted records (plus inline parse errors) into
/// `results` until `count` records have been collected, `max_walks`
/// records have been examined (when set), or EOF is reached.
///
/// Returns `(walked_bytes, eof)` so the caller can drive a progress
/// bar (walked_bytes covers rejected records too) and distinguish a
/// budget-exhausted partial scan from a true EOF: `eof` is `false`
/// when the scan stopped because `max_walks` was hit but the source
/// still has more records past `current_offset`.
fn scan_forward(
    file: &mut File,
    start_offset: u64,
    count: usize,
    max_walks: Option<usize>,
    filter: &Filter,
    results: &mut Vec<QueryRecord>,
) -> std::io::Result<(u64, bool)> {
    file.seek(SeekFrom::Start(start_offset))?;
    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    let mut current_offset = start_offset;
    let mut walks = 0usize;
    let mut eof = false;
    while results.len() < count {
        if let Some(max) = max_walks
            && walks >= max
        {
            break;
        }
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            eof = true;
            break;
        }
        walks += 1;
        let length = n as u64;
        let line = buf.trim_end_matches(['\r', '\n']);
        let parsed = serde_json::from_str::<Event>(line)
            .map_err(SourceError::from);
        push_if_accepted(
            results,
            ByteOffset(current_offset),
            length,
            parsed,
            line.to_string(),
            filter,
        );
        current_offset += length;
    }
    Ok((current_offset - start_offset, eof))
}

/// Walks `file` backward from `start_offset`, reading one record at
/// a time (from its trailing `\n` back to the previous `\n` or BOF)
/// and pushing accepted records (plus inline parse errors) into
/// `results` until `count` records have been collected, `max_walks`
/// records have been examined (when set), or BOF is reached.  Returns
/// `(walked_bytes, eof)` — see [`scan_forward`] for the meaning of
/// the `eof` flag.
fn scan_backward(
    file: &mut File,
    start_offset: u64,
    count: usize,
    max_walks: Option<usize>,
    filter: &Filter,
    results: &mut Vec<QueryRecord>,
) -> std::io::Result<(u64, bool)> {
    let mut cursor = start_offset;
    let mut walks = 0usize;
    let mut eof = false;
    while results.len() < count {
        if let Some(max) = max_walks
            && walks >= max
        {
            break;
        }
        if cursor == 0 {
            eof = true;
            break;
        }
        let (record_start, bytes) = read_record_before(file, cursor)?;
        let length = cursor - record_start;
        walks += 1;
        // Trim a single trailing `\r\n` or `\n` for parsing without
        // copying the bytes — `serde_json::from_slice` handles UTF-8
        // validation for us, so this stays bytes-only until parse.
        let mut content_end = bytes.len();
        while content_end > 0
            && matches!(bytes[content_end - 1], b'\n' | b'\r')
        {
            content_end -= 1;
        }
        let content = &bytes[..content_end];
        let parsed = serde_json::from_slice::<Event>(content)
            .map_err(SourceError::from);
        // Decode the trimmed bytes as UTF-8 for the raw view; lossy
        // conversion preserves the rest of the line when a stray byte
        // can't be decoded (rare in practice — JSON is UTF-8 by spec —
        // but cheaper than carrying `Vec<u8>` through the render path).
        let raw = String::from_utf8_lossy(content).into_owned();
        push_if_accepted(
            results,
            ByteOffset(record_start),
            length,
            parsed,
            raw,
            filter,
        );
        cursor = record_start;
    }
    Ok((start_offset - cursor, eof))
}

/// Pushes a [`QueryRecord`] for a parsed line into `results`,
/// skipping it only when the parse succeeded *and* the filter
/// rejects the resulting event.  Parse errors are always surfaced.
fn push_if_accepted(
    results: &mut Vec<QueryRecord>,
    offset: ByteOffset,
    length: u64,
    parsed: Result<Event, SourceError>,
    raw: String,
    filter: &Filter,
) {
    match parsed {
        Ok(event) => {
            if filter.matches(&event) {
                results.push(QueryRecord {
                    offset,
                    length,
                    event: Ok(event),
                    raw,
                });
            }
        }
        Err(e) => {
            results.push(QueryRecord {
                offset,
                length,
                event: Err(e),
                raw,
            });
        }
    }
}

/// Reads the record whose end is exactly at `offset` (exclusive).
///
/// Returns the record's start byte offset and its raw bytes,
/// including any trailing `\n`.  The caller is responsible for
/// stripping the line terminator before parsing.  Caller must ensure
/// `offset > 0`.
///
/// Walks backwards in 4 KiB chunks looking for the previous `\n`;
/// when none is found, the record extends to byte 0.  The byte at
/// position `offset - 1`, if it is a `\n`, is taken as the
/// terminator of *this* record and is excluded from the search for
/// the previous newline.
fn read_record_before(
    file: &mut File,
    offset: u64,
) -> std::io::Result<(u64, Vec<u8>)> {
    debug_assert!(offset > 0);

    const CHUNK: u64 = 4096;
    let mut search_end = offset.saturating_sub(1);
    let record_start: u64 = loop {
        if search_end == 0 {
            break 0;
        }
        let chunk_size = std::cmp::min(CHUNK, search_end);
        let chunk_start = search_end - chunk_size;
        file.seek(SeekFrom::Start(chunk_start))?;
        // `chunk_size` is bounded above by `CHUNK`, so this `as
        // usize` cannot truncate on any supported target.
        let mut buf = vec![0u8; chunk_size as usize];
        file.read_exact(&mut buf)?;
        if let Some(idx) = buf.iter().rposition(|&b| b == b'\n') {
            break chunk_start + (idx as u64) + 1;
        }
        search_end = chunk_start;
    };

    let length = usize::try_from(offset - record_start).map_err(|_| {
        std::io::Error::other("record too large to read into memory")
    })?;
    file.seek(SeekFrom::Start(record_start))?;
    let mut buf = vec![0u8; length];
    file.read_exact(&mut buf)?;
    Ok((record_start, buf))
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
    use crate::test_fixtures::{
        TestDir, append_bunyan, append_bunyan_at, append_raw, t,
    };
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

    // ----- excludes_all: metadata-based whole-file pruning -----

    fn metadata_with_name(name: &str) -> SourceMetadata {
        SourceMetadata {
            earliest: None,
            latest: None,
            name: Some(LoggerName::from(name.to_string())),
            hostname: None,
        }
    }

    #[test]
    fn excludes_all_name_match_does_not_prune() {
        let m = metadata_with_name("Nexus");
        let f: Filter = "name=Nexus".parse().unwrap();
        assert!(!m.excludes_all(&f));
    }

    #[test]
    fn excludes_all_name_mismatch_prunes() {
        let m = metadata_with_name("Nexus");
        let f: Filter = "name=SledAgent".parse().unwrap();
        assert!(m.excludes_all(&f));
    }

    #[test]
    fn excludes_all_negated_name_match_prunes() {
        let m = metadata_with_name("Nexus");
        let f: Filter = "name!=Nexus".parse().unwrap();
        assert!(m.excludes_all(&f));
    }

    #[test]
    fn excludes_all_negated_name_mismatch_does_not_prune() {
        let m = metadata_with_name("Nexus");
        let f: Filter = "name!=SledAgent".parse().unwrap();
        assert!(!m.excludes_all(&f));
    }

    #[test]
    fn excludes_all_hostname_mismatch_prunes() {
        let m = SourceMetadata {
            earliest: None,
            latest: None,
            name: None,
            hostname: Some(Hostname::from("h-1".to_string())),
        };
        let f: Filter = "hostname=h-2".parse().unwrap();
        assert!(m.excludes_all(&f));
    }

    #[test]
    fn excludes_all_unknown_predicates_are_ignored() {
        // Level, msg, extra-field predicates can't be evaluated against
        // metadata; they must never cause exclusion at this layer (the
        // per-record scan applies them).
        let m = metadata_with_name("Nexus");
        let f: Filter =
            "level>=warn msg=~boom component=nexus".parse().unwrap();
        assert!(!m.excludes_all(&f));
    }

    #[test]
    fn excludes_all_default_metadata_never_prunes() {
        // No first-record info means we know nothing about the source's
        // contents — be conservative and let the scan decide.
        let m = SourceMetadata::default();
        let f: Filter = "name=Nexus hostname=h".parse().unwrap();
        assert!(!m.excludes_all(&f));
    }

    // ----- query: forward direction -----

    /// Builds a multi-record fixture file at `path` with the supplied
    /// epoch-seconds.  Returns the file's total length so tests can
    /// query backward from EOF without an extra `metadata` call.
    fn write_fixture(path: &Utf8Path, name: &str, secs: &[i64]) -> u64 {
        for s in secs {
            append_bunyan_at(path, name, t(*s), &format!("m{s}"));
        }
        std::fs::metadata(path).unwrap().len()
    }

    #[test]
    fn query_forward_returns_records_in_order() {
        let dir = TestDir::new();
        let p = dir.path().join("fwd.log");
        write_fixture(&p, "Nexus", &[10, 20, 30]);
        let src = FileSource::open(&p).unwrap();
        let out = src
            .query(
                ByteOffset::ZERO,
                Direction::Forward,
                10,
                &Filter::default(),
            )
            .unwrap();
        let msgs: Vec<_> =
            out.iter().map(|r| r.event.as_ref().unwrap().msg.clone()).collect();
        assert_eq!(msgs, vec!["m10", "m20", "m30"]);
        dir.cleanup();
    }

    #[test]
    fn query_forward_respects_count() {
        let dir = TestDir::new();
        let p = dir.path().join("fwd_count.log");
        write_fixture(&p, "Nexus", &[10, 20, 30, 40]);
        let src = FileSource::open(&p).unwrap();
        let out = src
            .query(ByteOffset::ZERO, Direction::Forward, 2, &Filter::default())
            .unwrap();
        assert_eq!(out.len(), 2);
        let msgs: Vec<_> =
            out.iter().map(|r| r.event.as_ref().unwrap().msg.clone()).collect();
        assert_eq!(msgs, vec!["m10", "m20"]);
        dir.cleanup();
    }

    #[test]
    fn query_forward_from_eof_is_empty() {
        let dir = TestDir::new();
        let p = dir.path().join("fwd_eof.log");
        let len = write_fixture(&p, "Nexus", &[10, 20]);
        let src = FileSource::open(&p).unwrap();
        let out = src
            .query(
                ByteOffset::from(len),
                Direction::Forward,
                10,
                &Filter::default(),
            )
            .unwrap();
        assert!(out.is_empty());
        dir.cleanup();
    }

    #[test]
    fn query_forward_from_middle_resumes_at_offset() {
        // Pull the first record, take its end offset, and ask for the
        // rest from there — the second batch must start at the second
        // record, not repeat the first.
        let dir = TestDir::new();
        let p = dir.path().join("fwd_mid.log");
        write_fixture(&p, "Nexus", &[10, 20, 30]);
        let src = FileSource::open(&p).unwrap();
        let first = src
            .query(ByteOffset::ZERO, Direction::Forward, 1, &Filter::default())
            .unwrap();
        assert_eq!(first.len(), 1);
        let next_offset =
            ByteOffset::from(first[0].offset.get() + first[0].length);
        let rest = src
            .query(next_offset, Direction::Forward, 10, &Filter::default())
            .unwrap();
        let msgs: Vec<_> = rest
            .iter()
            .map(|r| r.event.as_ref().unwrap().msg.clone())
            .collect();
        assert_eq!(msgs, vec!["m20", "m30"]);
        dir.cleanup();
    }

    // ----- query: backward direction -----

    #[test]
    fn query_backward_from_eof_yields_records_in_reverse() {
        let dir = TestDir::new();
        let p = dir.path().join("bwd.log");
        let len = write_fixture(&p, "Nexus", &[10, 20, 30]);
        let src = FileSource::open(&p).unwrap();
        let out = src
            .query(
                ByteOffset::from(len),
                Direction::Backward,
                10,
                &Filter::default(),
            )
            .unwrap();
        let msgs: Vec<_> =
            out.iter().map(|r| r.event.as_ref().unwrap().msg.clone()).collect();
        assert_eq!(msgs, vec!["m30", "m20", "m10"]);
        dir.cleanup();
    }

    #[test]
    fn query_backward_respects_count() {
        let dir = TestDir::new();
        let p = dir.path().join("bwd_count.log");
        let len = write_fixture(&p, "Nexus", &[10, 20, 30, 40]);
        let src = FileSource::open(&p).unwrap();
        let out = src
            .query(
                ByteOffset::from(len),
                Direction::Backward,
                2,
                &Filter::default(),
            )
            .unwrap();
        let msgs: Vec<_> =
            out.iter().map(|r| r.event.as_ref().unwrap().msg.clone()).collect();
        assert_eq!(msgs, vec!["m40", "m30"]);
        dir.cleanup();
    }

    #[test]
    fn query_backward_from_bof_is_empty() {
        let dir = TestDir::new();
        let p = dir.path().join("bwd_bof.log");
        write_fixture(&p, "Nexus", &[10, 20]);
        let src = FileSource::open(&p).unwrap();
        let out = src
            .query(
                ByteOffset::ZERO,
                Direction::Backward,
                10,
                &Filter::default(),
            )
            .unwrap();
        assert!(out.is_empty());
        dir.cleanup();
    }

    #[test]
    fn query_backward_from_stale_offset_clamps_to_eof() {
        // A cursor past EOF (e.g. from a previous run, after the file
        // was rotated) should clamp to EOF rather than fail or
        // overrun.  Backward from `len + 100` returns the same records
        // as backward from `len`.
        let dir = TestDir::new();
        let p = dir.path().join("bwd_stale.log");
        let len = write_fixture(&p, "Nexus", &[10, 20]);
        let src = FileSource::open(&p).unwrap();
        let out = src
            .query(
                ByteOffset::from(len + 100),
                Direction::Backward,
                10,
                &Filter::default(),
            )
            .unwrap();
        let msgs: Vec<_> =
            out.iter().map(|r| r.event.as_ref().unwrap().msg.clone()).collect();
        assert_eq!(msgs, vec!["m20", "m10"]);
        dir.cleanup();
    }

    // ----- query: file-shape edge cases -----

    #[test]
    fn query_empty_file_both_directions() {
        let dir = TestDir::new();
        let p = dir.path().join("empty.log");
        std::fs::File::create(&p).unwrap();
        let src = FileSource::open(&p).unwrap();
        let fwd = src
            .query(
                ByteOffset::ZERO,
                Direction::Forward,
                10,
                &Filter::default(),
            )
            .unwrap();
        let bwd = src
            .query(
                ByteOffset::ZERO,
                Direction::Backward,
                10,
                &Filter::default(),
            )
            .unwrap();
        assert!(fwd.is_empty());
        assert!(bwd.is_empty());
        dir.cleanup();
    }

    #[test]
    fn query_no_trailing_newline_round_trip() {
        // Last record has no terminating `\n`.  Forward and backward
        // must each see both records with consistent offsets/lengths.
        let dir = TestDir::new();
        let p = dir.path().join("no_nl.log");
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
        let len = std::fs::metadata(&p).unwrap().len();
        let src = FileSource::open(&p).unwrap();

        let fwd = src
            .query(
                ByteOffset::ZERO,
                Direction::Forward,
                10,
                &Filter::default(),
            )
            .unwrap();
        let fwd_msgs: Vec<_> = fwd
            .iter()
            .map(|r| r.event.as_ref().unwrap().msg.clone())
            .collect();
        assert_eq!(fwd_msgs, vec!["first", "last"]);
        // The two record lengths sum to file length, matching the
        // "offset + length is the next record's start" contract.
        let total: u64 = fwd.iter().map(|r| r.length).sum();
        assert_eq!(total, len);

        let bwd = src
            .query(
                ByteOffset::from(len),
                Direction::Backward,
                10,
                &Filter::default(),
            )
            .unwrap();
        let bwd_msgs: Vec<_> = bwd
            .iter()
            .map(|r| r.event.as_ref().unwrap().msg.clone())
            .collect();
        assert_eq!(bwd_msgs, vec!["last", "first"]);
        dir.cleanup();
    }

    // ----- query: filter integration -----

    #[test]
    fn query_skips_records_rejected_by_filter() {
        // Filter accepts only `msg=m20`; the scan must walk past the
        // other two records without including them but still yield the
        // matching one.
        let dir = TestDir::new();
        let p = dir.path().join("filtered.log");
        write_fixture(&p, "Nexus", &[10, 20, 30]);
        let src = FileSource::open(&p).unwrap();
        let f: Filter = "msg=m20".parse().unwrap();
        let out = src
            .query(ByteOffset::ZERO, Direction::Forward, 10, &f)
            .unwrap();
        let msgs: Vec<_> =
            out.iter().map(|r| r.event.as_ref().unwrap().msg.clone()).collect();
        assert_eq!(msgs, vec!["m20"]);
        dir.cleanup();
    }

    #[test]
    fn query_excludes_all_short_circuits_before_io() {
        // Whole-source pruning by `name`: the filter cannot match this
        // file's recorded `name`.  Removing the file after open
        // confirms that `query` did not re-open it — any I/O attempt
        // would surface as `Err`.
        let dir = TestDir::new();
        let p = dir.path().join("short_circuit.log");
        append_bunyan_at(&p, "Nexus", t(10), "a");
        let src = FileSource::open(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        let f: Filter = "name=SledAgent".parse().unwrap();
        let out = src
            .query(ByteOffset::ZERO, Direction::Forward, 10, &f)
            .unwrap();
        assert!(out.is_empty());
        dir.cleanup();
    }

    #[test]
    fn query_count_zero_returns_empty() {
        // A `count = 0` request short-circuits without touching disk.
        let dir = TestDir::new();
        let p = dir.path().join("count_zero.log");
        write_fixture(&p, "Nexus", &[10, 20]);
        let src = FileSource::open(&p).unwrap();
        let out = src
            .query(
                ByteOffset::ZERO,
                Direction::Forward,
                0,
                &Filter::default(),
            )
            .unwrap();
        assert!(out.is_empty());
        dir.cleanup();
    }

    // ----- query: parse-error handling -----

    #[test]
    fn query_surfaces_parse_errors_inline() {
        // A non-JSON line in the middle of the file appears as an
        // inline `Err` record; the scan continues past it.
        let dir = TestDir::new();
        let p = dir.path().join("with_garbage.log");
        append_bunyan_at(&p, "Nexus", t(10), "a");
        append_raw(&p, "not json at all");
        append_bunyan_at(&p, "Nexus", t(20), "b");
        let src = FileSource::open(&p).unwrap();
        let out = src
            .query(
                ByteOffset::ZERO,
                Direction::Forward,
                10,
                &Filter::default(),
            )
            .unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].event.as_ref().unwrap().msg, "a");
        assert!(matches!(
            out[1].event.as_ref().unwrap_err(),
            SourceError::Parse(_),
        ));
        assert_eq!(out[2].event.as_ref().unwrap().msg, "b");
        dir.cleanup();
    }

    #[test]
    fn query_backward_surfaces_parse_errors_inline() {
        let dir = TestDir::new();
        let p = dir.path().join("with_garbage_bwd.log");
        append_bunyan_at(&p, "Nexus", t(10), "a");
        append_raw(&p, "not json");
        append_bunyan_at(&p, "Nexus", t(20), "b");
        let len = std::fs::metadata(&p).unwrap().len();
        let src = FileSource::open(&p).unwrap();
        let out = src
            .query(
                ByteOffset::from(len),
                Direction::Backward,
                10,
                &Filter::default(),
            )
            .unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].event.as_ref().unwrap().msg, "b");
        assert!(matches!(
            out[1].event.as_ref().unwrap_err(),
            SourceError::Parse(_),
        ));
        assert_eq!(out[2].event.as_ref().unwrap().msg, "a");
        dir.cleanup();
    }

    // ----- query: chunk-boundary backward read -----

    #[test]
    fn query_backward_handles_record_larger_than_chunk() {
        // The backward-read helper walks the file in 4 KiB chunks.
        // Make the second record's content larger than that to force
        // the search for the previous newline to span multiple chunks.
        let dir = TestDir::new();
        let p = dir.path().join("big_record_bwd.log");
        append_bunyan_at(&p, "Nexus", t(10), "small");
        let big = "x".repeat(10_000);
        append_bunyan_at(&p, "Nexus", t(20), &big);
        let len = std::fs::metadata(&p).unwrap().len();
        let src = FileSource::open(&p).unwrap();
        let out = src
            .query(
                ByteOffset::from(len),
                Direction::Backward,
                10,
                &Filter::default(),
            )
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event.as_ref().unwrap().msg, big);
        assert_eq!(out[1].event.as_ref().unwrap().msg, "small");
        dir.cleanup();
    }

    // ----- query: forward/backward symmetry -----

    #[test]
    fn query_forward_then_backward_round_trips() {
        // Read N records forward, then read N records backward starting
        // at where the forward scan stopped.  The two sequences should
        // be exact reverses of each other (offsets included).
        let dir = TestDir::new();
        let p = dir.path().join("round_trip.log");
        write_fixture(&p, "Nexus", &[10, 20, 30, 40, 50]);
        let src = FileSource::open(&p).unwrap();
        let fwd = src
            .query(
                ByteOffset::ZERO,
                Direction::Forward,
                100,
                &Filter::default(),
            )
            .unwrap();
        let last_end =
            fwd.last().map(|r| r.offset.get() + r.length).unwrap_or(0);
        let bwd = src
            .query(
                ByteOffset::from(last_end),
                Direction::Backward,
                100,
                &Filter::default(),
            )
            .unwrap();
        assert_eq!(fwd.len(), bwd.len());
        let fwd_offsets: Vec<_> = fwd.iter().map(|r| r.offset).collect();
        let bwd_offsets: Vec<_> = bwd.iter().map(|r| r.offset).collect();
        let mut fwd_rev = fwd_offsets.clone();
        fwd_rev.reverse();
        assert_eq!(bwd_offsets, fwd_rev);
        dir.cleanup();
    }

    #[test]
    fn query_offsets_partition_the_file() {
        // Record offsets and lengths from a forward scan should
        // partition the file exactly: no gaps, no overlaps, summing to
        // file length.
        let dir = TestDir::new();
        let p = dir.path().join("partition.log");
        let len = write_fixture(&p, "Nexus", &[10, 20, 30, 40, 50, 60]);
        let src = FileSource::open(&p).unwrap();
        let fwd = src
            .query(
                ByteOffset::ZERO,
                Direction::Forward,
                100,
                &Filter::default(),
            )
            .unwrap();
        let mut expected_offset = 0u64;
        for r in &fwd {
            assert_eq!(r.offset.get(), expected_offset);
            expected_offset += r.length;
        }
        assert_eq!(expected_offset, len);
        dir.cleanup();
    }
}
