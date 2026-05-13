// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests for the persistent-session pipeline.
//!
//! These exercise the public library API in the same shape the
//! `seer` binary uses it: create a session, save it through a
//! [`SessionStore`], mutate it the way `App` would, save again,
//! reload, and verify the state survived the round trip.  They
//! complement the per-module unit tests by checking that
//! [`SavePolicy`], [`SessionStore`], and [`Session`] interoperate
//! correctly at the library boundary — the same boundary the binary
//! sees.

use camino::Utf8PathBuf;
use camino_tempfile::tempdir;
use chrono::{TimeZone, Utc};
use std::time::{Duration, Instant};

use seer::{
    Bookmark, BookmarkId, BookmarkName, Cadence, Cursor, LogStream, MatchKind,
    SavePolicy, Session, SessionSource, SessionStore, SourceId,
};

/// Builds a [`SessionSource`] that doesn't refer to a real file on
/// disk.  Discovery only compares paths, so for tests that don't
/// open files these synthetic sources are sufficient.
fn synthetic_source(path: &str) -> SessionSource {
    SessionSource {
        id: SourceId::from(path.to_string()),
        path: Utf8PathBuf::from(path),
        mtime: Utc.timestamp_opt(0, 0).single().unwrap(),
        size: 0,
    }
}

fn synthetic_bookmark(msg: &str) -> Bookmark {
    Bookmark {
        id: BookmarkId::new_v4(),
        created_at: Utc::now(),
        cursor: Cursor::new(),
        name: None,
        display_source: SourceId::from("/log/a".to_string()),
        display_time: Utc::now(),
        display_msg: msg.to_string(),
    }
}

#[test]
fn fresh_session_round_trips_through_save_and_load() {
    let dir = tempdir().unwrap();
    let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
    let mut session = Session::new();
    session.sources.insert_unique(synthetic_source("/log/a")).unwrap();
    let id = session.id;

    store.save(id, &session).unwrap();
    let loaded = store.load(id).unwrap();
    assert_eq!(loaded.id, id);
    assert_eq!(loaded.sources.len(), 1);
    let first = loaded.sources.iter().next().expect("one source");
    assert_eq!(first.path, Utf8PathBuf::from("/log/a"));
}

#[test]
fn inline_save_pattern_keeps_disk_state_current() {
    // Simulates the App's inline-save loop: every "user gesture"
    // (here, just a function call) follows the
    // record(Inline) + store.save + mark_saved pattern.
    let dir = tempdir().unwrap();
    let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
    let mut policy = SavePolicy::new(SavePolicy::DEFAULT_DEBOUNCE);

    let mut session = Session::new();
    session.sources.insert_unique(synthetic_source("/log/a")).unwrap();
    let id = session.id;
    store.save(id, &session).unwrap();
    policy.mark_saved(Instant::now());
    assert!(!policy.dirty());

    // Open a new tab (the binary's Ctrl-T equivalent).
    let stream = LogStream::new("Tab 1".to_string());
    let stream_id = stream.id;
    session.streams.insert_unique(stream).unwrap();
    policy.record(Cadence::Inline);
    assert!(policy.dirty(), "Inline mutation must dirty the policy");
    store.save(id, &session).unwrap();
    policy.mark_saved(Instant::now());
    assert!(!policy.dirty());

    // Add a bookmark (the binary's `b` + dialog flow).
    let bookmark = synthetic_bookmark("marked");
    let bookmark_id = bookmark.id;
    session.add_bookmark(stream_id, bookmark);
    policy.record(Cadence::Inline);
    store.save(id, &session).unwrap();
    policy.mark_saved(Instant::now());

    // Reload and verify everything is preserved.
    let loaded = store.load(id).unwrap();
    assert_eq!(loaded.streams.len(), 1);
    let bms = loaded
        .user_bookmarks
        .get(&stream_id)
        .expect("bookmark bucket should round-trip");
    assert_eq!(bms.len(), 1);
    assert_eq!(bms[0].id, bookmark_id);
    assert_eq!(bms[0].display_msg, "marked");
    assert!(!policy.dirty());
}

#[test]
fn debounce_gates_writes_until_the_window_elapses() {
    let dir = tempdir().unwrap();
    let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
    let mut policy = SavePolicy::new(SavePolicy::DEFAULT_DEBOUNCE);

    let session = Session::new();
    let id = session.id;
    store.save(id, &session).unwrap();
    let t = Instant::now();
    policy.mark_saved(t);

    // High-cadence "scroll": dirty, not due yet.
    policy.record(Cadence::Debounced);
    assert!(policy.dirty());
    assert!(!policy.due(t));
    assert!(!policy.due(t + Duration::from_secs(5)));

    // Past the boundary, the App's `flush_if_due` would fire.
    assert!(policy.due(t + SavePolicy::DEFAULT_DEBOUNCE));

    // Simulate the flush.
    store.save(id, &session).unwrap();
    policy.mark_saved(t + SavePolicy::DEFAULT_DEBOUNCE);
    assert!(!policy.dirty());
}

#[test]
fn discovery_picks_overlapping_session_among_unrelated_ones() {
    // Save three sessions: one that overlaps the user's paths, one
    // that's an exact match, and one that's unrelated.
    // `find_matches` returns the two related ones, exact first.
    let dir = tempdir().unwrap();
    let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();

    let mut overlap = Session::new();
    overlap.sources.insert_unique(synthetic_source("/log/a")).unwrap();
    overlap.sources.insert_unique(synthetic_source("/log/c")).unwrap();
    store.save(overlap.id, &overlap).unwrap();

    let mut exact = Session::new();
    exact.sources.insert_unique(synthetic_source("/log/a")).unwrap();
    exact.sources.insert_unique(synthetic_source("/log/b")).unwrap();
    store.save(exact.id, &exact).unwrap();

    let mut unrelated = Session::new();
    unrelated.sources.insert_unique(synthetic_source("/log/x")).unwrap();
    store.save(unrelated.id, &unrelated).unwrap();

    let user_paths =
        vec![Utf8PathBuf::from("/log/a"), Utf8PathBuf::from("/log/b")];
    let matches = store.find_matches(&user_paths).unwrap();
    let ids: Vec<_> = matches.iter().map(|m| m.session.id).collect();
    let kinds: Vec<_> = matches.iter().map(|m| m.kind).collect();
    assert_eq!(ids, vec![exact.id, overlap.id]);
    assert_eq!(kinds, vec![MatchKind::Exact, MatchKind::Overlap]);
}

#[test]
fn resume_flow_recovers_user_bookmarks_and_streams_across_processes() {
    // First "process": build a session with streams and a bookmark
    // and save it.  Second "process" (new SessionStore handle):
    // discover and resume.  Bookmarks and streams should be intact.
    let dir = tempdir().unwrap();
    let sessions_dir = dir.path().join("sessions");
    let user_paths = vec![Utf8PathBuf::from("/log/a")];

    let saved_id;
    let stream_id;
    let bookmark_name;
    {
        let store = SessionStore::open_at(&sessions_dir).unwrap();
        let mut session = Session::new();
        session.sources.insert_unique(synthetic_source("/log/a")).unwrap();
        saved_id = session.id;

        let stream = LogStream::new("Tab 1".to_string());
        stream_id = stream.id;
        session.streams.insert_unique(stream).unwrap();

        let mut bookmark = synthetic_bookmark("captured");
        bookmark_name = BookmarkName::from("important".to_string());
        bookmark.name = Some(bookmark_name.clone());
        session.add_bookmark(stream_id, bookmark);

        store.save(saved_id, &session).unwrap();
    } // Drop the store + session — first "process" done.

    // Second "process": fresh handle on the same directory.
    let store = SessionStore::open_at(&sessions_dir).unwrap();
    let matches = store.find_matches(&user_paths).unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].kind, MatchKind::Exact);
    assert_eq!(matches[0].session.id, saved_id);

    let resumed = &matches[0].session;
    assert_eq!(resumed.streams.len(), 1);
    assert!(resumed.streams.get(&stream_id).is_some());
    assert_eq!(resumed.bookmark_count(), 1);
    let bms = resumed.user_bookmarks.get(&stream_id).unwrap();
    assert_eq!(bms[0].name.as_ref().unwrap(), &bookmark_name);
    assert_eq!(bms[0].display_msg, "captured");
}

#[test]
fn save_overwrites_prior_state_with_latest_content() {
    // Verifies the saver isn't append-only: a second save replaces
    // the first in place, so reload picks up the new content.
    let dir = tempdir().unwrap();
    let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
    let mut session = Session::new();
    let id = session.id;
    store.save(id, &session).unwrap();

    // Add three streams in three back-to-back saves.
    for i in 1..=3 {
        session
            .streams
            .insert_unique(LogStream::new(format!("Tab {i}")))
            .unwrap();
        store.save(id, &session).unwrap();
    }

    let loaded = store.load(id).unwrap();
    assert_eq!(loaded.streams.len(), 3);
}

#[test]
fn list_after_many_saves_returns_every_id() {
    let dir = tempdir().unwrap();
    let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
    let mut ids = Vec::new();
    for _ in 0..5 {
        let s = Session::new();
        ids.push(s.id);
        store.save(s.id, &s).unwrap();
    }
    let mut listed = store.list().unwrap();
    listed.sort();
    ids.sort();
    assert_eq!(listed, ids);
}
