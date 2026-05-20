// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Filesystem persistence for [`Session`] state.
//!
//! Sessions live under `$XDG_STATE_HOME/seer/sessions/` (resolved via
//! the [`etcetera`] crate's XDG strategy, which is what CLI tools
//! conventionally use on Linux and macOS).  The environment variable
//! `SEER_STATE_DIR` overrides the resolved state directory; tests use
//! it to redirect writes without touching `$HOME`.
//!
//! Each session is one JSON file whose stem is its
//! [`SessionId`](crate::session::SessionId).
//! Writes go through a sibling `.tmp` file plus a rename so a crash
//! mid-write can't leave a truncated session behind.
//!
//! This module owns the filesystem mechanics — paths, atomic write,
//! enumeration — and the path-set discovery that picks resumable
//! sessions out of the directory.

use crate::session::{Session, SessionId};
use camino::{Utf8Path, Utf8PathBuf};
use etcetera::{BaseStrategy, choose_base_strategy};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use thiserror::Error;

/// Environment variable that overrides the seer state directory.
///
/// When set, the session store uses `$SEER_STATE_DIR/sessions/`
/// instead of the XDG-derived path.  Tests use this to point seer
/// at a temp directory without touching `$HOME`.
pub const STATE_DIR_ENV: &str = "SEER_STATE_DIR";

/// Errors from the session store.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Could not resolve the seer state directory (no XDG state dir
    /// on this platform and `$SEER_STATE_DIR` was not set).
    #[error("could not resolve session state directory")]
    NoStateDir,

    /// The resolved state directory contains non-UTF-8 components,
    /// which seer's paths (built on [`Utf8PathBuf`]) cannot represent.
    #[error("session state directory contains non-UTF-8 path")]
    NonUtf8StateDir,

    /// An I/O operation on `path` failed.
    #[error("I/O error on {path}")]
    Io {
        /// Path the operation targeted.
        path: Utf8PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// The session file at `path` did not parse as JSON.
    #[error("could not parse session file {path}")]
    Parse {
        /// Path that failed to parse.
        path: Utf8PathBuf,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// Serializing a session to JSON failed.  Should not happen for
    /// well-formed `Session` values; included for completeness.
    #[error("could not serialize session")]
    Serialize(#[source] serde_json::Error),
}

/// Classification of how a saved session's source set relates to the
/// paths the user supplied on the command line.
///
/// Variants are declared in display order: sorting a `Vec<SessionMatch>`
/// by `kind` puts exact matches above supersets above overlaps, which
/// is the order the resume dialog will want.  Sessions that share no
/// path with the user are not classified at all — `find_matches` drops
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MatchKind {
    /// The session's sources are exactly the user's path set.
    Exact,
    /// The session's sources include every user path, plus extras.
    Superset,
    /// The session shares at least one source with the user, but the
    /// sets are neither equal nor is the session a strict superset.
    Overlap,
}

/// A saved session whose source set overlaps the paths the user
/// supplied on the command line.
///
/// Carries the full deserialized [`Session`] so the resume dialog can
/// render its preview row (id, last_saved_at, tab count, source count)
/// and resume into it without a second `load()`.
#[derive(Debug, Clone)]
pub struct SessionMatch {
    /// How this session's sources relate to the user's paths.
    pub kind: MatchKind,
    /// The session itself.
    pub session: Session,
}

/// Handle to the on-disk session directory.
///
/// Owns the absolute path of the sessions directory and ensures it
/// exists.  All read and write operations on session files go
/// through this type.
#[derive(Debug, Clone)]
pub struct SessionStore {
    sessions_dir: Utf8PathBuf,
}

impl SessionStore {
    /// Opens the store at the default location, creating the
    /// sessions directory if it does not already exist.
    ///
    /// Honors `$SEER_STATE_DIR` if set; otherwise resolves the state
    /// directory via etcetera's XDG strategy.
    pub fn open() -> Result<Self, StoreError> {
        let state_dir = resolve_state_dir(|k| std::env::var(k).ok())?;
        Self::open_at(state_dir.join("sessions"))
    }

    /// Opens a store rooted at a caller-supplied sessions directory,
    /// creating it if needed.  Mainly intended for tests.
    pub fn open_at(
        sessions_dir: impl Into<Utf8PathBuf>,
    ) -> Result<Self, StoreError> {
        let sessions_dir = sessions_dir.into();
        fs::create_dir_all(&sessions_dir).map_err(|source| StoreError::Io {
            path: sessions_dir.clone(),
            source,
        })?;
        Ok(Self { sessions_dir })
    }

    /// Returns the absolute path of the sessions directory.
    pub fn sessions_dir(&self) -> &Utf8Path {
        &self.sessions_dir
    }

    /// Returns the path that backs `id`.
    pub fn path_for(&self, id: SessionId) -> Utf8PathBuf {
        self.sessions_dir.join(format!("{id}.json"))
    }

    /// Loads the session with the given id.
    pub fn load(&self, id: SessionId) -> Result<Session, StoreError> {
        let path = self.path_for(id);
        let bytes = fs::read(&path)
            .map_err(|source| StoreError::Io { path: path.clone(), source })?;
        serde_json::from_slice(&bytes)
            .map_err(|source| StoreError::Parse { path, source })
    }

    /// Atomically saves `session` under `id`.
    ///
    /// Writes to a sibling `.tmp` file and renames into place.
    /// Rename is atomic on Unix, so a crash mid-write leaves at most
    /// a stale `.tmp` file; the in-place session file is either the
    /// previous good state or the new one.  No fsync — this is an
    /// editor-style write, not a database write.
    pub fn save(
        &self,
        id: SessionId,
        session: &Session,
    ) -> Result<(), StoreError> {
        let final_path = self.path_for(id);
        let tmp_path = self.sessions_dir.join(format!("{id}.json.tmp"));

        let body = serde_json::to_vec_pretty(session)
            .map_err(StoreError::Serialize)?;

        fs::write(&tmp_path, &body).map_err(|source| StoreError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        fs::rename(&tmp_path, &final_path)
            .map_err(|source| StoreError::Io { path: final_path, source })?;
        Ok(())
    }

    /// Returns the saved sessions whose source set overlaps
    /// `user_paths`, classified by overlap and ordered for display.
    ///
    /// `user_paths` should already be canonical (per
    /// [`std::fs::canonicalize`]); the session sources were captured
    /// canonical at open time, so the comparison is path-string
    /// equality.  Sessions whose JSON fails to parse are skipped —
    /// they would not be resumable anyway, and there is no useful
    /// action a caller could take with a half-loaded session.
    ///
    /// The returned matches are sorted exact first, then by
    /// `last_saved_at` descending.
    pub fn find_matches(
        &self,
        user_paths: &[Utf8PathBuf],
    ) -> Result<Vec<SessionMatch>, StoreError> {
        let user_set: BTreeSet<&Utf8Path> =
            user_paths.iter().map(|p| p.as_path()).collect();

        let ids = self.list()?;
        let mut matches = Vec::with_capacity(ids.len());
        for id in ids {
            // Parse failures are silently skipped — a session we can't
            // deserialize can't be resumed.  The file stays on disk so
            // a human can investigate.
            let Ok(session) = self.load(id) else { continue };
            let session_set: BTreeSet<&Utf8Path> =
                session.sources.iter().map(|s| s.path.as_path()).collect();
            if let Some(kind) = classify(&user_set, &session_set) {
                matches.push(SessionMatch { kind, session });
            }
        }

        // Sort matches in order that the user might want them.  That's
        // `MatchKind` order first (which prioritizes exact matches), then most
        // recent.
        matches.sort_by(|a, b| {
            a.kind.cmp(&b.kind).then_with(|| {
                b.session.last_saved_at.cmp(&a.session.last_saved_at)
            })
        });

        Ok(matches)
    }

    /// Enumerates session ids in the store.
    ///
    /// `.tmp` files (left over from a crashed save) and filenames
    /// that don't parse as a [`SessionId`] are silently skipped.
    /// The returned order is unspecified.
    pub fn list(&self) -> Result<Vec<SessionId>, StoreError> {
        let entries = fs::read_dir(&self.sessions_dir).map_err(|source| {
            StoreError::Io { path: self.sessions_dir.clone(), source }
        })?;
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                path: self.sessions_dir.clone(),
                source,
            })?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(stem) = name.strip_suffix(".json") else { continue };
            let Ok(id) = stem.parse::<SessionId>() else { continue };
            ids.push(id);
        }
        Ok(ids)
    }
}

/// Classifies a session against the user's command-line path set.
///
/// Returns `None` for sessions the caller should drop: either side
/// being empty, or no shared paths.
fn classify(
    user: &BTreeSet<&Utf8Path>,
    session: &BTreeSet<&Utf8Path>,
) -> Option<MatchKind> {
    if user.is_empty() || session.is_empty() {
        return None;
    }
    if user == session {
        return Some(MatchKind::Exact);
    }
    if user.is_subset(session) {
        // Sets aren't equal (checked above), so the session has every
        // user path plus at least one extra.
        return Some(MatchKind::Superset);
    }
    if !user.is_disjoint(session) {
        return Some(MatchKind::Overlap);
    }
    None
}

/// Resolves the seer state directory.
///
/// `env_lookup` is passed the env var name and returns its value if
/// set.  Production code threads `std::env::var(k).ok()` through; the
/// indirection lets tests exercise the env-var override path without
/// having to mutate the process environment.
fn resolve_state_dir(
    env_lookup: impl FnOnce(&str) -> Option<String>,
) -> Result<Utf8PathBuf, StoreError> {
    if let Some(override_dir) = env_lookup(STATE_DIR_ENV) {
        return Ok(Utf8PathBuf::from(override_dir));
    }
    let strategy =
        choose_base_strategy().map_err(|_| StoreError::NoStateDir)?;
    let state = strategy.state_dir().ok_or(StoreError::NoStateDir)?;
    let utf8 = Utf8PathBuf::from_path_buf(state)
        .map_err(|_| StoreError::NonUtf8StateDir)?;
    Ok(utf8.join("seer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Cursor;
    use crate::position::{ByteOffset, SourceId};
    use crate::session::{
        Session, SessionIdParseError, SessionSource, Tab, TabKind,
    };
    use crate::stream::LogStream;
    use crate::test_fixtures::t;
    use camino_tempfile::tempdir;

    fn session_with_one_tab() -> Session {
        let stream = LogStream::new("Tab 1".to_string());
        let stream_id = stream.id;
        let mut s = Session::new();
        s.streams.insert_unique(stream).expect("unique id");
        s.tabs.push(Tab {
            name: "Tab 1".to_string(),
            stream: stream_id,
            kind: TabKind::Stream,
            cursor: Some(Cursor::with([(
                SourceId::from("a.log".to_string()),
                ByteOffset::from(42_u64),
            )])),
        });
        s
    }

    /// Builds and saves a session with the given source paths and
    /// `last_saved_at` timestamp.  Returns the new session id.
    fn save_with_sources(
        store: &SessionStore,
        paths: &[&str],
        last_saved_at_secs: i64,
    ) -> SessionId {
        let mut s = Session::new();
        s.last_saved_at = t(last_saved_at_secs);
        for p in paths {
            s.sources
                .insert_unique(SessionSource {
                    id: SourceId::from((*p).to_string()),
                    path: Utf8PathBuf::from(*p),
                    mtime: t(0),
                    size: 0,
                })
                .expect("test inputs use unique paths");
        }
        let id = s.id;
        store.save(id, &s).unwrap();
        id
    }

    fn user_paths(paths: &[&str]) -> Vec<Utf8PathBuf> {
        paths.iter().map(|p| Utf8PathBuf::from(*p)).collect()
    }

    #[test]
    fn session_id_display_round_trips_through_parse() {
        let id = SessionId::random();
        let parsed: SessionId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn session_id_format_is_exactly_eight_lowercase_hex() {
        let id = SessionId::random();
        let s = id.to_string();
        assert_eq!(s.len(), 8);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(s, s.to_lowercase());
    }

    #[test]
    fn session_id_parse_rejects_wrong_length() {
        assert_eq!(
            "abc".parse::<SessionId>(),
            Err(SessionIdParseError::WrongLength(3))
        );
        assert_eq!(
            "abcdef0123".parse::<SessionId>(),
            Err(SessionIdParseError::WrongLength(10))
        );
    }

    #[test]
    fn session_id_parse_rejects_non_hex() {
        assert_eq!(
            "abcdefgh".parse::<SessionId>(),
            Err(SessionIdParseError::NonHex)
        );
    }

    #[test]
    fn session_id_serde_round_trip_as_string() {
        let id: SessionId = "deadbeef".parse().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"deadbeef\"");
        let back: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn save_then_load_round_trips_a_populated_session() {
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        let id = SessionId::random();
        let session = session_with_one_tab();

        store.save(id, &session).unwrap();
        let back = store.load(id).unwrap();
        assert_eq!(back.tabs.len(), session.tabs.len());
        assert_eq!(back.tabs[0].stream, session.tabs[0].stream);
        assert_eq!(back.tabs[0].cursor, session.tabs[0].cursor);
        assert_eq!(back.streams.len(), session.streams.len());
    }

    #[test]
    fn save_writes_pretty_json_to_id_dot_json() {
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        let id: SessionId = "deadbeef".parse().unwrap();
        store.save(id, &Session::new()).unwrap();

        let path = store.path_for(id);
        assert!(path.exists(), "expected {path} to exist");
        let body = std::fs::read_to_string(&path).unwrap();
        // Pretty-printed JSON has at least one newline.
        assert!(body.contains('\n'), "expected pretty JSON, got: {body}");
    }

    #[test]
    fn save_process() {
        // We don't have a way to actually crash mid-write inside a
        // test, but we can verify the contract: after a successful
        // save there is no `.tmp` file left behind, and the final
        // path is the one we expected.
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        let id = SessionId::random();
        store.save(id, &Session::new()).unwrap();

        let mut leftovers = Vec::new();
        for entry in std::fs::read_dir(store.sessions_dir()).unwrap() {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy().into_owned();
            if name.ends_with(".tmp") {
                leftovers.push(name);
            }
        }
        assert!(
            leftovers.is_empty(),
            "expected no .tmp leftovers, got {leftovers:?}"
        );
    }

    #[test]
    fn stale_tmp_file_from_prior_crash_does_not_corrupt_load() {
        // Simulate: a previous run crashed mid-save, leaving a
        // truncated `.tmp` file next to the real one.  load() and
        // list() should still see the real session and ignore the
        // tmp.
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        let id: SessionId = "12345678".parse().unwrap();
        store.save(id, &Session::new()).unwrap();

        let tmp_path = store.sessions_dir().join(format!("{id}.json.tmp"));
        std::fs::write(&tmp_path, "{ not valid").unwrap();

        // load() reads the real file, not the tmp.
        store.load(id).expect("load should ignore the tmp file");

        // list() does not include the tmp file as a session.
        let ids = store.list().unwrap();
        assert_eq!(ids, vec![id]);
    }

    #[test]
    fn list_returns_saved_ids_and_skips_unrelated_files() {
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        let a: SessionId = "00000001".parse().unwrap();
        let b: SessionId = "00000002".parse().unwrap();
        store.save(a, &Session::new()).unwrap();
        store.save(b, &Session::new()).unwrap();

        // Drop unrelated junk into the directory.
        std::fs::write(store.sessions_dir().join("not-a-session.txt"), "hi")
            .unwrap();
        std::fs::write(store.sessions_dir().join("badname.json"), "{}")
            .unwrap();

        let mut ids = store.list().unwrap();
        ids.sort();
        assert_eq!(ids, vec![a, b]);
    }

    #[test]
    fn load_on_missing_id_returns_io_error() {
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        let err = store.load(SessionId::random()).unwrap_err();
        assert!(matches!(err, StoreError::Io { .. }), "got {err:?}");
    }

    #[test]
    fn resolve_state_dir_uses_env_override_when_present() {
        let dir = tempdir().unwrap();
        let want = dir.path().to_path_buf();
        let got = resolve_state_dir(|k| {
            assert_eq!(k, STATE_DIR_ENV);
            Some(want.as_str().to_string())
        })
        .unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn resolve_state_dir_falls_back_to_xdg_when_env_absent() {
        // We can't assert the absolute path without depending on the
        // host's $HOME, but we can verify that the fallback path
        // ends in "/seer" — that's the contract we own.
        let got = resolve_state_dir(|_| None).unwrap();
        assert!(
            got.ends_with("seer"),
            "expected fallback path to end in 'seer', got {got}"
        );
    }

    #[test]
    fn find_matches_classifies_exact_match() {
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        let id = save_with_sources(&store, &["/log/a", "/log/b"], 100);

        let matches =
            store.find_matches(&user_paths(&["/log/a", "/log/b"])).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, MatchKind::Exact);
        assert_eq!(matches[0].session.id, id);

        // Order of user paths should not matter.
        let matches =
            store.find_matches(&user_paths(&["/log/b", "/log/a"])).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, MatchKind::Exact);
    }

    #[test]
    fn find_matches_classifies_superset() {
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        save_with_sources(&store, &["/log/a", "/log/b", "/log/c"], 100);

        let matches =
            store.find_matches(&user_paths(&["/log/a", "/log/b"])).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, MatchKind::Superset);
    }

    #[test]
    fn find_matches_classifies_overlap() {
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        // Session has b and c; user asks for a and b.  Sets overlap on
        // b but neither contains the other.
        save_with_sources(&store, &["/log/b", "/log/c"], 100);

        let matches =
            store.find_matches(&user_paths(&["/log/a", "/log/b"])).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, MatchKind::Overlap);
    }

    #[test]
    fn find_matches_skips_disjoint_sessions() {
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        save_with_sources(&store, &["/log/a"], 100);

        let matches = store.find_matches(&user_paths(&["/log/b"])).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn find_matches_returns_empty_for_empty_user_paths() {
        // With no user paths, no session can overlap.  The caller (the
        // resume dialog) is responsible for treating an empty path
        // list as "show all sessions" if that's what it wants.
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        save_with_sources(&store, &["/log/a"], 100);

        let matches = store.find_matches(&[]).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn find_matches_skips_sessions_with_no_sources() {
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        // An empty session (e.g. one created but never opened against
        // a file) can't overlap with any user paths.
        save_with_sources(&store, &[], 100);

        let matches = store.find_matches(&user_paths(&["/log/a"])).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn find_matches_returns_empty_when_store_is_empty() {
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        let matches = store.find_matches(&user_paths(&["/log/a"])).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn find_matches_orders_by_kind_then_recency() {
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();

        // For user query [a, x]:
        //   exact_old, exact_new  — sources [a, x]      → Exact
        //   superset              — sources [a, x, b]   → Superset
        //   overlap               — sources [a, c]      → Overlap
        //                                                 (shares a but
        //                                                 not x, and not
        //                                                 a superset)
        //   disjoint              — sources [d]         → skipped
        //
        // Note: the Superset and Overlap rows have *newer*
        // last_saved_at than the Exact rows.  Kind should still take
        // precedence over recency.
        let exact_old = save_with_sources(&store, &["/log/a", "/log/x"], 100);
        let exact_new = save_with_sources(&store, &["/log/a", "/log/x"], 200);
        let superset =
            save_with_sources(&store, &["/log/a", "/log/x", "/log/b"], 500);
        let overlap = save_with_sources(&store, &["/log/a", "/log/c"], 400);
        let _disjoint = save_with_sources(&store, &["/log/d"], 600);

        let matches =
            store.find_matches(&user_paths(&["/log/a", "/log/x"])).unwrap();
        let got: Vec<(MatchKind, SessionId)> =
            matches.iter().map(|m| (m.kind, m.session.id)).collect();
        assert_eq!(
            got,
            vec![
                (MatchKind::Exact, exact_new),
                (MatchKind::Exact, exact_old),
                (MatchKind::Superset, superset),
                (MatchKind::Overlap, overlap),
            ],
            "exact first (newest within kind), then superset, then \
             overlap; the disjoint session is dropped entirely"
        );
    }

    #[test]
    fn find_matches_skips_corrupt_session_files() {
        let dir = tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
        let good = save_with_sources(&store, &["/log/a"], 100);

        // Drop a corrupt file named like a session into the
        // directory.  list() will surface its id; load() will fail;
        // find_matches() should silently skip it.
        std::fs::write(
            store.sessions_dir().join("deadbeef.json"),
            "{ not valid",
        )
        .unwrap();

        let matches = store.find_matches(&user_paths(&["/log/a"])).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].session.id, good);
        assert_eq!(matches[0].kind, MatchKind::Exact);
    }
}
