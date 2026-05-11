// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Persistent session state for the TUI.
//!
//! A `Session` captures everything needed to re-open the user's view of
//! their investigation: the open tabs and where each is scrolled to, the
//! set of log streams that exist (whether or not a tab is currently
//! showing them), and the user's named/unnamed bookmarks indexed by
//! stream.  Sources, named filter groups, and the set of fields shown by
//! each stream will join this struct as those concepts land.
//!
//! ## Schema versioning
//!
//! `Session::version` is the schema version persisted to disk.  New
//! fields should always be `#[serde(default)]` so older session files
//! deserialize cleanly into newer code; restructuring or renaming an
//! existing field is what bumps the version and requires a migration
//! shim keyed on `version`.

use crate::engine::Cursor;
use crate::session_store::SessionId;
use crate::source::SourceId;
use crate::stream::{LogStream, LogStreamId, LogStreamPosition};
use camino::Utf8PathBuf;
use chrono::{DateTime, Utc};
use derive_more::{Display, From};
use iddqd::IdOrdMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Current on-disk schema version.  Bump whenever the serialized
/// shape of a [`Session`] changes — the `schemars`-derived schema
/// fixture test will fail and prompt the author to refresh both
/// this constant and the checked-in fixture.
pub const CURRENT_SESSION_VERSION: u32 = 3;

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
    /// First slice of the bookmarked event's `msg`, cached so the
    /// Bookmarks tab can show it without re-querying the engine.
    pub display_msg: String,
}

/// A tab in the TUI.
///
/// A tab is a view onto exactly one [`LogStream`] (referenced by id; the
/// stream lives in [`Session::streams`]).  `cursor` is the position the
/// tab is currently scrolled to.  `cursor` is `None` for an
/// empty-or-unrendered tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Tab {
    pub stream: LogStreamId,
    #[serde(default)]
    pub cursor: Option<LogStreamPosition>,
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

/// Top-level session state.
///
/// Designed to be the unit of persistence: serialize this and you've
/// captured enough to put the user back where they left off.
///
/// Fields are not `#[serde(default)]`: there are no live session
/// files on disk to be backwards-compatible with, and silently
/// defaulting in fields would defeat the schema-tripwire test that
/// guards `CURRENT_SESSION_VERSION`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    /// On-disk schema version.  Bumped on changes that aren't
    /// otherwise serde-compatible (renames, restructured fields).
    pub version: u32,
    /// Short id; also the filename stem on disk.
    pub id: SessionId,
    /// Sources this session was opened against.
    pub sources: Vec<SessionSource>,
    /// When the session was first created.
    pub created_at: DateTime<Utc>,
    /// When the saver last successfully wrote this session to disk.
    pub last_saved_at: DateTime<Utc>,
    /// PID of the seer process that most recently saved this
    /// session.  Recorded for diagnostics (e.g. a future
    /// concurrent-access warning); not consulted for correctness.
    pub last_pid: u32,
    pub tabs: Vec<Tab>,
    /// Log streams owned by this session, keyed by id.  Serialized
    /// as a JSON array because [`IdOrdMap`] writes itself that way;
    /// the explicit `schemars(with = …)` is what surfaces that to
    /// the schema generator since `IdOrdMap` has no `JsonSchema`
    /// impl of its own.
    #[schemars(with = "Vec<LogStream>")]
    pub streams: IdOrdMap<LogStream>,
    pub user_bookmarks: BTreeMap<LogStreamId, Vec<Bookmark>>,
}

impl Session {
    /// Returns a fresh session with a new random id, the current
    /// time, and no sources/tabs/bookmarks.
    ///
    /// Mints a new id and timestamps on every call, so there is no
    /// `Default` impl — that contract would conflict with "default
    /// values are deterministic".
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let now = Utc::now();
        Self {
            version: CURRENT_SESSION_VERSION,
            id: SessionId::random(),
            sources: Vec::new(),
            created_at: now,
            last_saved_at: now,
            last_pid: std::process::id(),
            tabs: Vec::new(),
            streams: IdOrdMap::new(),
            user_bookmarks: BTreeMap::new(),
        }
    }

    /// Inserts `bookmark` into `user_bookmarks` under `stream`.
    pub fn add_bookmark(&mut self, stream: LogStreamId, bookmark: Bookmark) {
        self.user_bookmarks.entry(stream).or_default().push(bookmark);
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
            if let Some(idx) = bms.iter().position(|b| b.id == id) {
                bms.remove(idx);
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
    use crate::source::{ByteOffset, SourceId};
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    fn position(secs: i64) -> LogStreamPosition {
        LogStreamPosition::new(SourceId::from("a.log".to_string()), t(secs), 0)
    }

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
        s.tabs.push(Tab { stream: stream_id, cursor: Some(position(42)) });
        s.user_bookmarks.insert(
            stream_id,
            vec![make_bookmark(0, Some("start")), make_bookmark(100, None)],
        );

        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();

        assert_eq!(back.version, CURRENT_SESSION_VERSION);
        assert_eq!(back.tabs.len(), 1);
        assert_eq!(back.tabs[0].stream, stream_id);
        assert_eq!(back.tabs[0].cursor, Some(position(42)));

        assert_eq!(back.streams.len(), 1);
        assert!(back.streams.get(&stream_id).is_some());

        let bms = back.user_bookmarks.get(&stream_id).unwrap();
        assert_eq!(bms.len(), 2);
        assert_eq!(
            bms[0].name.as_ref().map(|n| n.to_string()),
            Some("start".to_string())
        );
        assert_eq!(bms[0].cursor, cursor_at(0));
        assert_eq!(bms[1].name, None);
        assert_eq!(bms[1].cursor, cursor_at(100 * 100));
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
}
