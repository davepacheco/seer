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
//! Each session is one JSON file whose stem is its [`SessionId`].
//! Writes go through a sibling `.tmp` file plus a rename so a crash
//! mid-write can't leave a truncated session behind.
//!
//! This module is filesystem mechanics only.  Higher-level concerns
//! (session discovery, save policy) live elsewhere — see
//! `plan-sessions.md` for the layering.

use crate::session::Session;
use camino::{Utf8Path, Utf8PathBuf};
use etcetera::{BaseStrategy, choose_base_strategy};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

/// Environment variable that overrides the seer state directory.
///
/// When set, the session store uses `$SEER_STATE_DIR/sessions/`
/// instead of the XDG-derived path.  Tests use this to point seer
/// at a temp directory without touching `$HOME`.
pub const STATE_DIR_ENV: &str = "SEER_STATE_DIR";

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

    /// Returns the id as its four underlying bytes.
    pub fn as_bytes(&self) -> [u8; 4] {
        self.0
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
        std::borrow::Cow::Borrowed("seer::session_store::SessionId")
    }

    fn json_schema(
        _: &mut schemars::r#gen::SchemaGenerator,
    ) -> schemars::schema::Schema {
        schemars::schema::SchemaObject {
            instance_type: Some(
                schemars::schema::InstanceType::String.into(),
            ),
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
        let bytes = fs::read(&path).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
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
        fs::rename(&tmp_path, &final_path).map_err(|source| StoreError::Io {
            path: final_path,
            source,
        })?;
        Ok(())
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
    use crate::session::{Session, Tab};
    use crate::stream::{LogStream, LogStreamPosition};
    use crate::source::SourceId;
    use camino_tempfile::tempdir;
    use chrono::{TimeZone, Utc};

    fn session_with_one_tab() -> Session {
        let stream = LogStream::new("Tab 1".to_string());
        let stream_id = stream.id;
        let mut s = Session::new();
        s.streams.insert_unique(stream).expect("unique id");
        s.tabs.push(Tab {
            stream: stream_id,
            cursor: Some(LogStreamPosition::new(
                SourceId::from("a.log".to_string()),
                Utc.timestamp_opt(42, 0).single().unwrap(),
                0,
            )),
        });
        s
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
    fn save_is_atomic_via_tmp_plus_rename() {
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

        let tmp_path = store
            .sessions_dir()
            .join(format!("{id}.json.tmp"));
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
        std::fs::write(
            store.sessions_dir().join("not-a-session.txt"),
            "hi",
        )
        .unwrap();
        std::fs::write(
            store.sessions_dir().join("badname.json"),
            "{}",
        )
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
}
