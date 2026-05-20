// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Persistent session state for the TUI.
//!
//! A `Session` captures everything needed to re-open the user's view of
//! their investigation: the open tabs and where each is scrolled to, the
//! set of log streams that exist (whether or not a tab is currently
//! showing them), and the user's named/unnamed bookmarks indexed by
//! stream.
//!
//! ## Schema versioning
//!
//! `Session::version` is the schema version persisted to disk.  New
//! fields can be `#[serde(default)]` so that older session files deserialize
//! cleanly into newer code; restructuring or renaming an existing field is what
//! bumps the version and requires a migration shim keyed on `version`.

use crate::engine::Cursor;
use crate::position::SourceId;
use crate::stream::{LogStream, LogStreamId};
use camino::Utf8PathBuf;
use chrono::{DateTime, Utc};
use derive_more::{Display, From};
use iddqd::{IdOrdItem, IdOrdMap, id_upcast};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

/// Current on-disk schema version.  Bump whenever the serialized
/// shape of a [`Session`] changes — the `schemars`-derived schema
/// fixture test will fail and prompt the author to refresh both
/// this constant and the checked-in fixture.
pub const CURRENT_SESSION_VERSION: u32 = 1;

/// Short, user-typeable session id.
///
/// Eight lowercase hex characters drawn from the first four bytes of
/// a UUIDv4.  Long enough that collisions are vanishingly rare in a
/// single user's session directory; short enough to type after
/// `--resume`.  The id is the filename stem on disk: `<id>.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId([u8; 4]);

impl SessionId {
    /// Returns a freshly-generated random session id.
    pub fn random() -> Self {
        let bytes = Uuid::new_v4().into_bytes();
        Self([bytes[0], bytes[1], bytes[2], bytes[3]])
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3]
        )
    }
}

/// Error parsing a [`SessionId`] from a string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SessionIdParseError {
    /// Input was not 8 characters long.
    #[error("session id must be 8 hex characters; got {0}")]
    WrongLength(usize),

    /// Input contained a non-hex character.
    #[error("session id contains a non-hex character")]
    NonHex,
}

impl FromStr for SessionId {
    type Err = SessionIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 8 {
            return Err(SessionIdParseError::WrongLength(s.len()));
        }
        let mut out = [0u8; 4];
        for (i, byte) in out.iter_mut().enumerate() {
            let chunk = &s[i * 2..i * 2 + 2];
            *byte = u8::from_str_radix(chunk, 16)
                .map_err(|_| SessionIdParseError::NonHex)?;
        }
        Ok(Self(out))
    }
}

// Serialize as the 8-char hex string the user sees, not as a byte
// array — so the on-disk representation matches what `--resume`
// takes on the command line.
impl Serialize for SessionId {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// Manual JsonSchema impl: SessionId serializes as the 8-char hex
// string from `Display`, not as its underlying 4-byte array, so the
// schema must describe a string with the right shape rather than
// inheriting the byte-array shape a derive would produce.
impl schemars::JsonSchema for SessionId {
    fn schema_name() -> String {
        "SessionId".to_owned()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("seer::session::SessionId")
    }

    fn json_schema(
        _: &mut schemars::r#gen::SchemaGenerator,
    ) -> schemars::schema::Schema {
        schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::String.into()),
            string: Some(Box::new(schemars::schema::StringValidation {
                pattern: Some(r"^[0-9a-f]{8}$".to_owned()),
                min_length: Some(8),
                max_length: Some(8),
            })),
            ..Default::default()
        }
        .into()
    }
}

/// User-supplied name for a bookmark.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Display,
    From,
    JsonSchema,
)]
#[serde(transparent)]
pub struct BookmarkName(String);

/// Stable identifier for a [`Bookmark`].
///
/// Unlike a position, the id never changes: the Bookmarks tab keys its
/// selection and delete operations on this so a renaming or reordering
/// of bookmarks doesn't surprise the user.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Display,
    From,
    JsonSchema,
)]
#[serde(transparent)]
pub struct BookmarkId(Uuid);

impl BookmarkId {
    /// Returns a freshly-generated random id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

/// A user-created bookmark: a deliberate landmark in a log stream.
///
/// A bookmark refers to its target by a byte-offset [`Cursor`] — feeding
/// the cursor to [`crate::StreamView::seek_to_cursor`] lands the viewport
/// on the bookmarked record.  This makes navigation `O(1)` (no walk from
/// byte 0) and is stable across filter changes: a filter can hide the
/// bookmarked event from view, but the cursor still names the same byte
/// position regardless of what the active filter is.
///
/// `display_time` and `display_msg` are captured at creation time so the
/// Bookmarks tab can render the row even when the source isn't currently
/// loaded — and so the preview reflects what the user saw when they made
/// the bookmark, not whatever the file looks like now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Bookmark {
    pub id: BookmarkId,
    pub created_at: DateTime<Utc>,
    pub cursor: Cursor,
    #[serde(default)]
    pub name: Option<BookmarkName>,
    /// Source the bookmarked event came from, captured at creation
    /// time so the Bookmarks tab can show its filename without
    /// re-resolving the cursor against the engine.
    pub display_source: SourceId,
    /// Timestamp of the bookmarked event, cached so the Bookmarks tab
    /// can show it without re-querying the engine.
    pub display_time: DateTime<Utc>,
    /// Bunyan `name` field of the bookmarked event (typically the
    /// component, e.g. `Nexus`), cached so the Bookmarks tab can show
    /// it without re-querying the engine.
    pub display_name: String,
    /// First slice of the bookmarked event's `msg`, cached so the
    /// Bookmarks tab can show it without re-querying the engine.
    pub display_msg: String,
}

impl IdOrdItem for Bookmark {
    type Key<'a> = BookmarkId;

    fn key(&self) -> Self::Key<'_> {
        self.id
    }

    id_upcast!();
}

/// What a [`Tab`] displays.
///
/// Today this is a binary distinction: regular log-record view versus
/// the field/time histogram summary.  Persisted on each tab so a
/// resumed session reopens both kinds in the right shape.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum TabKind {
    /// Regular per-record log view.
    Stream,
    /// Histogram summary of the active filter's events.
    Summary,
}

/// A tab in the TUI.
///
/// A tab is a view onto exactly one [`LogStream`] (referenced by id; the
/// stream lives in [`Session::streams`]).  The tab's `name` is what the
/// tab strip shows and what `--tab` matches against; it is independent
/// of the backing stream's name so renaming a tab does not also rename
/// the stream (and two tabs targeting the same stream can carry
/// distinct names).  `cursor` is the byte-offset [`Cursor`] the tab is
/// currently scrolled to, captured at save time so a resumed session
/// can land the viewport on the same record.  `cursor` is `None` for
/// an empty-or-unrendered tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Tab {
    pub name: String,
    pub stream: LogStreamId,
    pub kind: TabKind,
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

/// A source the session was opened against.
///
/// Captures the canonical path the user supplied plus a lightweight
/// fingerprint (mtime + size) taken at open time.  At resume time, a
/// mismatching fingerprint signals that the underlying file has
/// changed and any cached parse state for it should be invalidated.
/// The fingerprint is also the seed for any future path-independent
/// matching (e.g. a re-extracted support tarball whose paths
/// differ).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSource {
    /// id used to refer to this source from elsewhere in the
    /// session (cursors, bookmarks, log streams)
    pub id: SourceId,
    /// canonical path captured at open time
    #[schemars(with = "String")]
    pub path: Utf8PathBuf,
    /// file modification time captured at open time
    pub mtime: DateTime<Utc>,
    /// file size in bytes captured at open time
    pub size: u64,
}

impl IdOrdItem for SessionSource {
    type Key<'a> = SourceId;

    fn key(&self) -> Self::Key<'_> {
        self.id.clone()
    }

    id_upcast!();
}

/// Top-level session state.
///
/// Designed to be the unit of persistence: serialize this and you've captured
/// enough to put the user back where they left off.
///
/// For now, fields are not `#[serde(default)]`: there are no live session files
/// on disk to be backwards-compatible with, and silently defaulting in fields
/// would defeat the schema-tripwire test that guards `CURRENT_SESSION_VERSION`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    /// On-disk schema version.  Bumped on changes that aren't
    /// otherwise serde-compatible (renames, restructured fields).
    pub version: u32,
    /// Short id; also the filename stem on disk.
    pub id: SessionId,
    /// Sources this session was opened against.
    pub sources: IdOrdMap<SessionSource>,
    /// When the session was first created.
    pub created_at: DateTime<Utc>,
    /// When the saver last successfully wrote this session to disk.
    pub last_saved_at: DateTime<Utc>,
    /// PID of the seer process that most recently saved this
    /// session.  Recorded for diagnostics (e.g. a future
    /// concurrent-access warning); not consulted for correctness.
    pub last_pid: u32,
    pub tabs: Vec<Tab>,
    /// Log streams owned by this session, keyed by id.
    pub streams: IdOrdMap<LogStream>,
    /// Bookmarks indexed by the stream they target.
    pub user_bookmarks: BTreeMap<LogStreamId, IdOrdMap<Bookmark>>,
    /// Recently submitted search patterns, most-recently-used first.
    /// Capped at [`MAX_SEARCH_HISTORY`].  Populated by successful
    /// search submissions in the TUI; used to drive the search
    /// prompt's history navigation (Up/Down).
    pub search_history: Vec<String>,
}

/// Maximum number of distinct search patterns retained in
/// [`Session::search_history`].  When the history is at capacity and a
/// new pattern is recorded, the oldest entry falls off the end.
pub const MAX_SEARCH_HISTORY: usize = 30;

impl Session {
    /// Returns a fresh session with a new random id, the current
    /// time, and no sources/tabs/bookmarks.
    ///
    /// Mints a new id and timestamps on every call, so there is no
    /// `Default` impl — that contract would conflict with "default
    /// values are deterministic".
    pub fn new() -> Self {
        let now = Utc::now();
        Self {
            version: CURRENT_SESSION_VERSION,
            id: SessionId::random(),
            sources: IdOrdMap::new(),
            created_at: now,
            last_saved_at: now,
            last_pid: std::process::id(),
            tabs: Vec::new(),
            streams: IdOrdMap::new(),
            user_bookmarks: BTreeMap::new(),
            search_history: Vec::new(),
        }
    }

    /// Records `pattern` as the most recently used search.
    ///
    /// If an equal entry already exists it is moved to the front
    /// (rather than duplicated); if the history is at
    /// [`MAX_SEARCH_HISTORY`] the oldest entry is dropped.  Empty
    /// patterns are not recorded.
    pub fn record_search(&mut self, pattern: &str) {
        if pattern.is_empty() {
            return;
        }
        if let Some(pos) = self.search_history.iter().position(|p| p == pattern)
        {
            self.search_history.remove(pos);
        }
        self.search_history.insert(0, pattern.to_string());
        self.search_history.truncate(MAX_SEARCH_HISTORY);
    }

    /// Inserts `bookmark` into `user_bookmarks` under `stream`.
    pub fn add_bookmark(&mut self, stream: LogStreamId, bookmark: Bookmark) {
        self.user_bookmarks
            .entry(stream)
            .or_default()
            .insert_unique(bookmark)
            .expect("bookmark ids are freshly minted at creation time");
    }

    /// Removes the bookmark with the given id, returning `true` if it
    /// was found.  When removing a bookmark leaves a stream's bucket
    /// empty, the bucket is removed too — `user_bookmarks.is_empty()`
    /// then becomes the synthetic Bookmarks tab's "should I exist?"
    /// signal.
    pub fn remove_bookmark(&mut self, id: BookmarkId) -> bool {
        let mut empty_streams: Vec<LogStreamId> = Vec::new();
        let mut removed = false;
        for (stream_id, bms) in self.user_bookmarks.iter_mut() {
            if bms.remove(&id).is_some() {
                removed = true;
                if bms.is_empty() {
                    empty_streams.push(*stream_id);
                }
                break;
            }
        }
        for s in empty_streams {
            self.user_bookmarks.remove(&s);
        }
        removed
    }

    /// Total bookmark count across every stream.  The Bookmarks tab is
    /// rendered iff this is non-zero.
    pub fn bookmark_count(&self) -> usize {
        self.user_bookmarks.values().map(|v| v.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::{ByteOffset, SourceId};
    use crate::test_fixtures::t;

    fn cursor_at(offset: u64) -> Cursor {
        Cursor::with([(
            SourceId::from("a.log".to_string()),
            ByteOffset::from(offset),
        )])
    }

    fn make_bookmark(secs: i64, name: Option<&str>) -> Bookmark {
        Bookmark {
            id: BookmarkId::new_v4(),
            created_at: t(0),
            cursor: cursor_at(secs as u64 * 100),
            name: name.map(|s| BookmarkName::from(s.to_string())),
            display_source: SourceId::from("a.log".to_string()),
            display_time: t(secs),
            display_name: "Nexus".to_string(),
            display_msg: format!("msg @ {secs}"),
        }
    }

    #[test]
    fn empty_session_round_trips_through_serde() {
        let s = Session::new();
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, CURRENT_SESSION_VERSION);
        assert!(back.tabs.is_empty());
        assert!(back.streams.is_empty());
        assert!(back.user_bookmarks.is_empty());
    }

    #[test]
    fn populated_session_round_trips_through_serde() {
        let stream = LogStream::new("Tab 1".to_string());
        let stream_id = stream.id;

        let mut s = Session::new();
        s.streams.insert_unique(stream).expect("unique id");
        s.tabs.push(Tab {
            name: "Tab 1".to_string(),
            stream: stream_id,
            kind: TabKind::Stream,
            cursor: Some(cursor_at(42 * 100)),
        });
        s.add_bookmark(stream_id, make_bookmark(0, Some("start")));
        s.add_bookmark(stream_id, make_bookmark(100, None));

        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();

        assert_eq!(back.version, CURRENT_SESSION_VERSION);
        assert_eq!(back.tabs.len(), 1);
        assert_eq!(back.tabs[0].stream, stream_id);
        assert_eq!(back.tabs[0].kind, TabKind::Stream);
        assert_eq!(back.tabs[0].cursor, Some(cursor_at(42 * 100)));

        assert_eq!(back.streams.len(), 1);
        assert!(back.streams.get(&stream_id).is_some());

        let bms = back.user_bookmarks.get(&stream_id).unwrap();
        assert_eq!(bms.len(), 2);
        let by_name: Vec<&Bookmark> = bms.iter().collect();
        let start = by_name
            .iter()
            .find(|b| {
                b.name.as_ref().map(|n| n.to_string()).as_deref()
                    == Some("start")
            })
            .expect("named bookmark round-trips");
        assert_eq!(start.cursor, cursor_at(0));
        let unnamed = by_name
            .iter()
            .find(|b| b.name.is_none())
            .expect("unnamed bookmark round-trips");
        assert_eq!(unnamed.cursor, cursor_at(100 * 100));
    }

    #[test]
    fn new_session_has_fresh_id_and_current_timestamps() {
        let before = Utc::now();
        let s = Session::new();
        let after = Utc::now();

        // Two consecutive calls produce different ids.
        let other = Session::new();
        assert_ne!(s.id, other.id);

        // Timestamps land within the call window.
        assert!(s.created_at >= before && s.created_at <= after);
        assert_eq!(s.created_at, s.last_saved_at);
        assert_eq!(s.last_pid, std::process::id());
        assert_eq!(s.version, CURRENT_SESSION_VERSION);
    }

    #[test]
    fn add_bookmark_inserts_into_per_stream_bucket() {
        let mut s = Session::new();
        let stream_id = LogStreamId::new_v4();
        s.add_bookmark(stream_id, make_bookmark(0, None));
        s.add_bookmark(stream_id, make_bookmark(1, Some("named")));
        assert_eq!(s.bookmark_count(), 2);
        assert_eq!(s.user_bookmarks.get(&stream_id).unwrap().len(), 2);
    }

    #[test]
    fn remove_bookmark_drops_empty_bucket() {
        let mut s = Session::new();
        let stream_id = LogStreamId::new_v4();
        let bm = make_bookmark(0, None);
        let id = bm.id;
        s.add_bookmark(stream_id, bm);
        assert!(s.remove_bookmark(id));
        assert!(s.user_bookmarks.is_empty());
        assert_eq!(s.bookmark_count(), 0);
    }

    #[test]
    fn remove_unknown_bookmark_returns_false() {
        let mut s = Session::new();
        assert!(!s.remove_bookmark(BookmarkId::new_v4()));
    }

    #[test]
    fn record_search_orders_most_recent_first() {
        let mut s = Session::new();
        s.record_search("alpha");
        s.record_search("beta");
        s.record_search("gamma");
        assert_eq!(s.search_history, vec!["gamma", "beta", "alpha"]);
    }

    #[test]
    fn record_search_moves_duplicates_to_front_without_duplicating() {
        let mut s = Session::new();
        s.record_search("alpha");
        s.record_search("beta");
        s.record_search("alpha");
        assert_eq!(s.search_history, vec!["alpha", "beta"]);
    }

    #[test]
    fn record_search_caps_at_max_history() {
        let mut s = Session::new();
        for i in 0..(MAX_SEARCH_HISTORY + 5) {
            s.record_search(&format!("p{i}"));
        }
        assert_eq!(s.search_history.len(), MAX_SEARCH_HISTORY);
        // Most recent entry sits at the front; oldest five fell off.
        assert_eq!(s.search_history[0], format!("p{}", MAX_SEARCH_HISTORY + 4));
        assert_eq!(s.search_history[MAX_SEARCH_HISTORY - 1], format!("p{}", 5));
    }

    #[test]
    fn record_search_ignores_empty_patterns() {
        let mut s = Session::new();
        s.record_search("");
        assert!(s.search_history.is_empty());
    }
}
