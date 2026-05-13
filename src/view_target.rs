// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Resolves a `(session id, selector)` pair into the concrete inputs
//! needed to reproduce a saved view: source paths, filter, render
//! options, starting cursor, and emission mode.
//!
//! This is the shared back-end behind `seeit`'s session mode and the
//! `seer` keybinding that prints a `seeit` command for the active
//! view.  The TUI's tab-restore path covers similar ground but is
//! interleaved with engine setup and UI bookkeeping; resolution here
//! returns a plain data record so non-TUI callers can use it
//! without dragging in ratatui.

use crate::engine::Cursor;
use crate::filter::Filter;
use crate::render::RenderOpts;
use crate::session::{
    Bookmark, BookmarkId, BookmarkName, Session, SessionId, TabKind,
};
use crate::session_store::{SessionStore, StoreError};
use crate::stream::{LogStream, LogStreamId};
use camino::Utf8PathBuf;
use chrono::{DateTime, Utc};
use thiserror::Error;

/// Which view a `seeit` invocation should reproduce out of a saved
/// [`Session`].
///
/// Variants correspond one-to-one with the `--stream` / `--tab` /
/// `--bookmark` selector flags; [`Selector::WholeSession`] is the
/// "no selector supplied" case where `seeit` emits every event from
/// every source the session knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// Every event from every source, with no filter and library
    /// default render options.
    WholeSession,
    /// The log stream whose [`LogStream::name`] matches the given
    /// string exactly.
    Stream(String),
    /// The tab whose backing stream has the given name.  The tab's
    /// saved cursor (or start-of-stream if none) is the starting
    /// position.
    Tab(String),
    /// The bookmark whose [`BookmarkName`] matches the given string
    /// exactly, or whose [`BookmarkId`] display form (an 8-4-4-4-12
    /// UUID) starts with the given string.
    Bookmark(String),
}

/// What `seeit` should emit for a resolved target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedMode {
    /// One rendered event per output block.
    Records,
    /// A summary histogram over the filtered events.
    Summary,
}

/// The pieces a `seeit` invocation needs to reproduce a saved view.
///
/// The caller is responsible for installing `sources` into an engine,
/// constructing a stepper from `cursor` and `filter`, and rendering
/// with `render_opts` (or building a summary, when `mode` is
/// [`ResolvedMode::Summary`]).  CLI-level filter and render
/// overrides layer on top of these values.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// Canonical paths of the sources to open, in the order they
    /// appear in [`Session::sources`].
    pub sources: Vec<Utf8PathBuf>,
    /// Filter to apply to each event.
    pub filter: Filter,
    /// Field-visibility settings to render each event with.
    pub render_opts: RenderOpts,
    /// Starting cursor for emission.  `Cursor::default()` for "start
    /// of merged stream".
    pub cursor: Cursor,
    /// Whether to emit records or a summary histogram.
    pub mode: ResolvedMode,
}

/// One element of an ambiguous-match error's candidate list.
///
/// Carries both the bookmark id (always present) and the optional
/// user-supplied name so the CLI error message can show whichever the
/// user is likely to recognize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkChoice {
    /// Stable id of the candidate bookmark.
    pub id: BookmarkId,
    /// User-supplied name, if any.
    pub name: Option<BookmarkName>,
}

/// Errors returned by [`resolve`] and [`resolve_in_session`].
#[derive(Debug, Error)]
pub enum ResolveError {
    /// The session store failed (typically I/O or a parse error on a
    /// session file).
    #[error(transparent)]
    Store(#[from] StoreError),

    /// No log stream in the session matches the given name.
    #[error("no log stream named {name:?} in session {session}")]
    UnknownStream {
        /// id of the session that was searched
        session: SessionId,
        /// name the caller asked for
        name: String,
    },

    /// More than one log stream in the session matches the given
    /// name; the duplicate names are listed for the user.
    #[error(
        "ambiguous stream name {name:?} in session {session}: \
         matches {} streams", candidates.len()
    )]
    AmbiguousStream {
        /// id of the session that was searched
        session: SessionId,
        /// name the caller asked for
        name: String,
        /// matching stream ids (names are all equal to `name`)
        candidates: Vec<LogStreamId>,
    },

    /// No tab in the session matches the given name.
    #[error("no tab named {name:?} in session {session}")]
    UnknownTab {
        /// id of the session that was searched
        session: SessionId,
        /// name the caller asked for
        name: String,
    },

    /// More than one tab in the session matches the given name.
    #[error(
        "ambiguous tab name {name:?} in session {session}: \
         matches {} tabs", candidates.len()
    )]
    AmbiguousTab {
        /// id of the session that was searched
        session: SessionId,
        /// name the caller asked for
        name: String,
        /// matching tab indices (in `Session::tabs` order)
        candidates: Vec<usize>,
    },

    /// No bookmark in the session matches the given name or id
    /// prefix.
    #[error("no bookmark matching {needle:?} in session {session}")]
    UnknownBookmark {
        /// id of the session that was searched
        session: SessionId,
        /// name or id prefix the caller asked for
        needle: String,
    },

    /// More than one bookmark matches the given needle.
    #[error(
        "ambiguous bookmark {needle:?} in session {session}: \
         {} matches", candidates.len()
    )]
    AmbiguousBookmark {
        /// id of the session that was searched
        session: SessionId,
        /// name or id prefix the caller asked for
        needle: String,
        /// matching candidates
        candidates: Vec<BookmarkChoice>,
    },

    /// A source listed in the session no longer matches its
    /// fingerprint on disk.  Treated as a hard error per design:
    /// resuming against changed bytes would produce output that
    /// doesn't match what the user saw in `seer`.
    #[error(
        "source {path} has changed since the session was saved: \
         expected size {expected_size}, mtime {expected_mtime}; \
         actual size {actual_size}, mtime {actual_mtime}"
    )]
    SourceFingerprint {
        /// canonical path of the changed source
        path: Utf8PathBuf,
        /// size captured when the session was opened
        expected_size: u64,
        /// mtime captured when the session was opened
        expected_mtime: DateTime<Utc>,
        /// size on disk now
        actual_size: u64,
        /// mtime on disk now
        actual_mtime: DateTime<Utc>,
    },

    /// A source listed in the session is missing from disk.
    #[error("source {path} from session {session} is missing on disk")]
    SourceMissing {
        /// id of the session that was searched
        session: SessionId,
        /// canonical path of the missing source
        path: Utf8PathBuf,
    },

    /// A bookmark referenced a log stream that no longer exists in
    /// the session.  Should not arise during normal use — the TUI
    /// drops a stream's bookmarks when the stream goes away — but
    /// the resolver checks defensively to surface session-file
    /// corruption rather than silently picking the wrong filter.
    #[error(
        "bookmark {bookmark} references unknown stream {stream} \
         in session {session}"
    )]
    BookmarkStreamMissing {
        /// id of the session that was searched
        session: SessionId,
        /// id of the bookmark with the dangling reference
        bookmark: BookmarkId,
        /// id of the stream the bookmark pointed at
        stream: LogStreamId,
    },
}

/// Loads `session_id` from `store` and resolves `selector` against
/// it.
///
/// Equivalent to [`resolve_in_session`] called with a freshly-loaded
/// session, broken out so callers that already have a `Session` in
/// hand can avoid the redundant load.
pub fn resolve(
    store: &SessionStore,
    session_id: SessionId,
    selector: &Selector,
) -> Result<ResolvedTarget, ResolveError> {
    let session = store.load(session_id)?;
    resolve_in_session(&session, selector)
}

/// Resolves `selector` against an already-loaded `session`.
///
/// The source fingerprint check runs regardless of the selector —
/// any session-mode emission must read the on-disk files, so a
/// changed file is a problem for every selector.
pub fn resolve_in_session(
    session: &Session,
    selector: &Selector,
) -> Result<ResolvedTarget, ResolveError> {
    check_source_fingerprints(session)?;
    let sources: Vec<Utf8PathBuf> =
        session.sources.iter().map(|s| s.path.clone()).collect();

    match selector {
        Selector::WholeSession => Ok(ResolvedTarget {
            sources,
            filter: Filter::default(),
            render_opts: RenderOpts::default(),
            cursor: Cursor::default(),
            mode: ResolvedMode::Records,
        }),
        Selector::Stream(name) => {
            let stream = find_stream(session, name)?;
            Ok(ResolvedTarget {
                sources,
                filter: stream.filter.clone(),
                render_opts: stream.render_opts(),
                cursor: Cursor::default(),
                mode: ResolvedMode::Records,
            })
        }
        Selector::Tab(name) => {
            let (stream, kind, cursor) = find_tab(session, name)?;
            Ok(ResolvedTarget {
                sources,
                filter: stream.filter.clone(),
                render_opts: stream.render_opts(),
                cursor: cursor.unwrap_or_default(),
                mode: match kind {
                    TabKind::Stream => ResolvedMode::Records,
                    TabKind::Summary => ResolvedMode::Summary,
                },
            })
        }
        Selector::Bookmark(needle) => {
            let (stream, bookmark) = find_bookmark(session, needle)?;
            Ok(ResolvedTarget {
                sources,
                filter: stream.filter.clone(),
                render_opts: stream.render_opts(),
                cursor: bookmark.cursor.clone(),
                mode: ResolvedMode::Records,
            })
        }
    }
}

/// Looks up the unique log stream whose name equals `name`.
///
/// Returns [`ResolveError::UnknownStream`] for no match,
/// [`ResolveError::AmbiguousStream`] for more than one.
fn find_stream<'a>(
    session: &'a Session,
    name: &str,
) -> Result<&'a LogStream, ResolveError> {
    let matches: Vec<&LogStream> =
        session.streams.iter().filter(|s| s.name == name).collect();
    match matches.as_slice() {
        [] => Err(ResolveError::UnknownStream {
            session: session.id,
            name: name.to_owned(),
        }),
        [only] => Ok(*only),
        many => Err(ResolveError::AmbiguousStream {
            session: session.id,
            name: name.to_owned(),
            candidates: many.iter().map(|s| s.id).collect(),
        }),
    }
}

/// Looks up the unique tab whose own name equals `name`.
///
/// Returns the backing stream, the tab's [`TabKind`], and its saved
/// cursor (if any).  Tabs that reference a stream the session no
/// longer owns are skipped — the same defensive behavior as the TUI's
/// tab-restore path.
fn find_tab<'a>(
    session: &'a Session,
    name: &str,
) -> Result<(&'a LogStream, TabKind, Option<Cursor>), ResolveError> {
    // Collect indices so the ambiguity error can name them.
    let matches: Vec<(usize, &LogStream, TabKind, Option<Cursor>)> = session
        .tabs
        .iter()
        .enumerate()
        .filter_map(|(idx, tab)| {
            let stream = session.streams.get(&tab.stream)?;
            (tab.name == name)
                .then(|| (idx, stream, tab.kind, tab.cursor.clone()))
        })
        .collect();
    match matches.as_slice() {
        [] => Err(ResolveError::UnknownTab {
            session: session.id,
            name: name.to_owned(),
        }),
        [(_, stream, kind, cursor)] => Ok((stream, *kind, cursor.clone())),
        many => Err(ResolveError::AmbiguousTab {
            session: session.id,
            name: name.to_owned(),
            candidates: many.iter().map(|(idx, _, _, _)| *idx).collect(),
        }),
    }
}

/// Looks up the unique bookmark matching `needle`, by name or id
/// prefix.
///
/// Matching is exact on `BookmarkName`; for ids it checks whether the
/// id's display form starts with `needle`.  An empty `needle` would
/// match every bookmark — the function rejects that as
/// `UnknownBookmark` rather than ambiguity, since "no needle" is a
/// caller mistake rather than a request to disambiguate.
fn find_bookmark<'a>(
    session: &'a Session,
    needle: &str,
) -> Result<(&'a LogStream, &'a Bookmark), ResolveError> {
    if needle.is_empty() {
        return Err(ResolveError::UnknownBookmark {
            session: session.id,
            needle: needle.to_owned(),
        });
    }
    let mut candidates: Vec<(LogStreamId, &Bookmark)> = Vec::new();
    for (stream_id, bms) in &session.user_bookmarks {
        for bm in bms {
            let matches_name = bm
                .name
                .as_ref()
                .map(|n| n.to_string() == needle)
                .unwrap_or(false);
            let matches_id = bm.id.to_string().starts_with(needle);
            if matches_name || matches_id {
                candidates.push((*stream_id, bm));
            }
        }
    }
    match candidates.as_slice() {
        [] => Err(ResolveError::UnknownBookmark {
            session: session.id,
            needle: needle.to_owned(),
        }),
        [(stream_id, bm)] => {
            let stream = session.streams.get(stream_id).ok_or(
                ResolveError::BookmarkStreamMissing {
                    session: session.id,
                    bookmark: bm.id,
                    stream: *stream_id,
                },
            )?;
            Ok((stream, *bm))
        }
        many => Err(ResolveError::AmbiguousBookmark {
            session: session.id,
            needle: needle.to_owned(),
            candidates: many
                .iter()
                .map(|(_, bm)| BookmarkChoice {
                    id: bm.id,
                    name: bm.name.clone(),
                })
                .collect(),
        }),
    }
}

/// Verifies that every source listed in the session still exists on
/// disk and still has the size + mtime the session captured at open
/// time.
///
/// Per the design, a mismatch is a hard error: the cursors and
/// bookmarks in the session are byte offsets, so a file whose bytes
/// have shifted can't be navigated faithfully.  An override flag for
/// "use anyway" is intentionally not provided in this phase.
fn check_source_fingerprints(session: &Session) -> Result<(), ResolveError> {
    for s in &session.sources {
        let metadata = match std::fs::metadata(&s.path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ResolveError::SourceMissing {
                    session: session.id,
                    path: s.path.clone(),
                });
            }
            Err(e) => {
                return Err(ResolveError::Store(StoreError::Io {
                    path: s.path.clone(),
                    source: e,
                }));
            }
        };
        let actual_size = metadata.len();
        let actual_mtime: DateTime<Utc> = match metadata.modified() {
            Ok(m) => m.into(),
            Err(e) => {
                return Err(ResolveError::Store(StoreError::Io {
                    path: s.path.clone(),
                    source: e,
                }));
            }
        };
        if actual_size != s.size || actual_mtime != s.mtime {
            return Err(ResolveError::SourceFingerprint {
                path: s.path.clone(),
                expected_size: s.size,
                expected_mtime: s.mtime,
                actual_size,
                actual_mtime,
            });
        }
    }
    Ok(())
}

/// Builds a shell-quotable `seeit` invocation that reproduces the
/// `seer` view targeted by `selector` in session `session_id`.
///
/// Produces the minimal correct command — for selector-based
/// targets, all the stream's filter and render-options come along
/// inside the saved session, so the command itself stays short.  The
/// only argument that needs shell-aware quoting is the selector's
/// name (tabs and streams may contain spaces); [`shlex::try_quote`]
/// handles that, with a fallback to ASCII double-quotes for the
/// (currently impossible) case where the name contains a NUL.
pub fn build_seeit_command(
    session_id: SessionId,
    selector: &Selector,
) -> String {
    match selector {
        Selector::WholeSession => {
            format!("seeit --session {session_id}")
        }
        Selector::Stream(name) => {
            format!(
                "seeit --session {session_id} --stream {}",
                shell_quote(name),
            )
        }
        Selector::Tab(name) => {
            format!("seeit --session {session_id} --tab {}", shell_quote(name),)
        }
        Selector::Bookmark(needle) => {
            format!(
                "seeit --session {session_id} --bookmark {}",
                shell_quote(needle),
            )
        }
    }
}

/// Shell-quotes `s` for inclusion in a `seeit` invocation.  Falls
/// back to bare double-quoting if `shlex` rejects the input (only
/// happens for embedded NULs today, but the fallback keeps the
/// function total).
fn shell_quote(s: &str) -> String {
    match shlex::try_quote(s) {
        Ok(cow) => cow.into_owned(),
        Err(_) => format!("\"{s}\""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Cursor;
    use crate::position::{ByteOffset, SourceId};
    use crate::session::{Bookmark, BookmarkId, BookmarkName, Session, Tab};
    use crate::stream::LogStream;
    use camino_tempfile::tempdir;
    use chrono::TimeZone;
    use std::fs;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    /// Writes `body` to `dir/<name>` and returns a [`SessionSource`]
    /// whose fingerprint reflects the file as it sits on disk now.
    fn write_source(
        dir: &camino::Utf8Path,
        name: &str,
        body: &str,
    ) -> crate::session::SessionSource {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        crate::session::SessionSource {
            id: SourceId::from(path.as_str().to_string()),
            path,
            mtime: metadata.modified().unwrap().into(),
            size: metadata.len(),
        }
    }

    /// Builds a session whose `sources` point at on-disk files in
    /// `dir`, with one stream and one tab.  The stream's filter is
    /// the default (matches everything); callers tweak after the fact.
    fn fixture_session(dir: &camino::Utf8Path) -> Session {
        let mut s = Session::new();
        s.sources.insert_unique(write_source(dir, "a.log", "{}\n")).unwrap();
        s.sources.insert_unique(write_source(dir, "b.log", "{}\n")).unwrap();
        let stream = LogStream::new("Tab 1".to_string());
        let stream_id = stream.id;
        s.streams.insert_unique(stream).unwrap();
        s.tabs.push(Tab {
            name: "Tab 1".to_string(),
            stream: stream_id,
            kind: TabKind::Stream,
            cursor: Some(Cursor::with([(
                SourceId::from(dir.join("a.log").as_str().to_string()),
                ByteOffset::from(3_u64),
            )])),
        });
        s
    }

    fn bookmark_at(name: Option<&str>, source: &str) -> Bookmark {
        Bookmark {
            id: BookmarkId::new_v4(),
            created_at: t(0),
            cursor: Cursor::with([(
                SourceId::from(source.to_string()),
                ByteOffset::from(1_u64),
            )]),
            name: name.map(|n| BookmarkName::from(n.to_string())),
            display_source: SourceId::from(source.to_string()),
            display_time: t(1),
            display_msg: "msg".to_string(),
        }
    }

    #[test]
    fn whole_session_resolves_to_default_filter_and_render_opts() {
        let dir = tempdir().unwrap();
        let session = fixture_session(dir.path());

        let t = resolve_in_session(&session, &Selector::WholeSession).unwrap();
        assert_eq!(t.sources.len(), 2);
        assert_eq!(t.mode, ResolvedMode::Records);
        assert_eq!(t.cursor, Cursor::default());
        // Filter::default has no predicates.
        assert!(t.filter.predicates().is_empty());
        assert_eq!(t.render_opts, RenderOpts::default());
    }

    #[test]
    fn stream_resolves_to_stream_filter_and_render_opts() {
        let dir = tempdir().unwrap();
        let mut session = fixture_session(dir.path());
        // Tweak the lone stream so the resolver picks up something
        // distinct from defaults.  Remove + mutate + reinsert is the
        // idiom the rest of the codebase uses to mutate
        // [`IdOrdMap`] entries (see `seer.rs`'s filter dialog
        // handlers).
        let stream_id = session.streams.iter().next().unwrap().id;
        let mut stream = session.streams.remove(&stream_id).unwrap();
        stream.show_extras = true;
        stream.filter = "level>=warn".parse().unwrap();
        session.streams.insert_unique(stream).unwrap();

        let t = resolve_in_session(
            &session,
            &Selector::Stream("Tab 1".to_string()),
        )
        .unwrap();
        assert_eq!(t.cursor, Cursor::default(), "stream starts at beginning");
        assert!(t.render_opts.show_extras);
        assert_eq!(t.filter.predicates().len(), 1);
        assert_eq!(t.mode, ResolvedMode::Records);
    }

    #[test]
    fn tab_resolves_to_saved_cursor() {
        let dir = tempdir().unwrap();
        let session = fixture_session(dir.path());
        let expected_cursor =
            session.tabs[0].cursor.clone().expect("fixture sets cursor");

        let t =
            resolve_in_session(&session, &Selector::Tab("Tab 1".to_string()))
                .unwrap();
        assert_eq!(t.cursor, expected_cursor);
        assert_eq!(t.mode, ResolvedMode::Records);
    }

    #[test]
    fn tab_with_no_saved_cursor_starts_at_beginning() {
        let dir = tempdir().unwrap();
        let mut session = fixture_session(dir.path());
        session.tabs[0].cursor = None;

        let t =
            resolve_in_session(&session, &Selector::Tab("Tab 1".to_string()))
                .unwrap();
        assert_eq!(t.cursor, Cursor::default());
    }

    #[test]
    fn summary_tab_resolves_to_summary_mode() {
        let dir = tempdir().unwrap();
        let mut session = fixture_session(dir.path());
        session.tabs[0].kind = TabKind::Summary;

        let t =
            resolve_in_session(&session, &Selector::Tab("Tab 1".to_string()))
                .unwrap();
        assert_eq!(t.mode, ResolvedMode::Summary);
    }

    #[test]
    fn bookmark_resolves_by_name() {
        let dir = tempdir().unwrap();
        let mut session = fixture_session(dir.path());
        let stream_id = session.tabs[0].stream;
        let bm = bookmark_at(Some("panic"), dir.path().join("a.log").as_str());
        let expected_cursor = bm.cursor.clone();
        session.add_bookmark(stream_id, bm);

        let t = resolve_in_session(
            &session,
            &Selector::Bookmark("panic".to_string()),
        )
        .unwrap();
        assert_eq!(t.cursor, expected_cursor);
    }

    #[test]
    fn bookmark_resolves_by_id_prefix() {
        let dir = tempdir().unwrap();
        let mut session = fixture_session(dir.path());
        let stream_id = session.tabs[0].stream;
        let bm = bookmark_at(None, dir.path().join("a.log").as_str());
        let id_str = bm.id.to_string();
        let expected_cursor = bm.cursor.clone();
        session.add_bookmark(stream_id, bm);

        let prefix = &id_str[..8];
        let t = resolve_in_session(
            &session,
            &Selector::Bookmark(prefix.to_string()),
        )
        .unwrap();
        assert_eq!(t.cursor, expected_cursor);
    }

    #[test]
    fn unknown_stream_errors() {
        let dir = tempdir().unwrap();
        let session = fixture_session(dir.path());
        let err =
            resolve_in_session(&session, &Selector::Stream("nope".to_string()))
                .unwrap_err();
        match err {
            ResolveError::UnknownStream { name, .. } => {
                assert_eq!(name, "nope");
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn ambiguous_stream_lists_candidates() {
        let dir = tempdir().unwrap();
        let mut session = fixture_session(dir.path());
        // Add a second stream with the same name as the first.
        let duplicate = LogStream::new("Tab 1".to_string());
        session.streams.insert_unique(duplicate).unwrap();

        let err = resolve_in_session(
            &session,
            &Selector::Stream("Tab 1".to_string()),
        )
        .unwrap_err();
        match err {
            ResolveError::AmbiguousStream { candidates, .. } => {
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn unknown_tab_errors() {
        let dir = tempdir().unwrap();
        let session = fixture_session(dir.path());
        let err =
            resolve_in_session(&session, &Selector::Tab("Nope".to_string()))
                .unwrap_err();
        assert!(matches!(err, ResolveError::UnknownTab { .. }));
    }

    #[test]
    fn unknown_bookmark_errors() {
        let dir = tempdir().unwrap();
        let session = fixture_session(dir.path());
        let err = resolve_in_session(
            &session,
            &Selector::Bookmark("nope".to_string()),
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::UnknownBookmark { .. }));
    }

    #[test]
    fn empty_bookmark_needle_is_unknown_not_ambiguous() {
        let dir = tempdir().unwrap();
        let mut session = fixture_session(dir.path());
        let stream_id = session.tabs[0].stream;
        session.add_bookmark(
            stream_id,
            bookmark_at(Some("x"), dir.path().join("a.log").as_str()),
        );

        let err =
            resolve_in_session(&session, &Selector::Bookmark(String::new()))
                .unwrap_err();
        assert!(matches!(err, ResolveError::UnknownBookmark { .. }));
    }

    #[test]
    fn ambiguous_bookmark_lists_candidates() {
        let dir = tempdir().unwrap();
        let mut session = fixture_session(dir.path());
        let stream_id = session.tabs[0].stream;
        session.add_bookmark(
            stream_id,
            bookmark_at(Some("dup"), dir.path().join("a.log").as_str()),
        );
        session.add_bookmark(
            stream_id,
            bookmark_at(Some("dup"), dir.path().join("a.log").as_str()),
        );

        let err = resolve_in_session(
            &session,
            &Selector::Bookmark("dup".to_string()),
        )
        .unwrap_err();
        match err {
            ResolveError::AmbiguousBookmark { candidates, .. } => {
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn fingerprint_mismatch_is_a_hard_error() {
        let dir = tempdir().unwrap();
        let session = fixture_session(dir.path());
        // Touch a.log so its mtime + size shift.
        fs::write(dir.path().join("a.log"), "{}\n{}\n").unwrap();

        let err =
            resolve_in_session(&session, &Selector::WholeSession).unwrap_err();
        match err {
            ResolveError::SourceFingerprint {
                path,
                expected_size,
                actual_size,
                ..
            } => {
                assert!(path.as_str().ends_with("a.log"));
                assert_ne!(expected_size, actual_size);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn missing_source_errors_specifically() {
        let dir = tempdir().unwrap();
        let session = fixture_session(dir.path());
        fs::remove_file(dir.path().join("a.log")).unwrap();

        let err =
            resolve_in_session(&session, &Selector::WholeSession).unwrap_err();
        assert!(matches!(err, ResolveError::SourceMissing { .. }));
    }

    #[test]
    fn build_command_for_whole_session_omits_selector() {
        let id: SessionId = "deadbeef".parse().unwrap();
        assert_eq!(
            build_seeit_command(id, &Selector::WholeSession),
            "seeit --session deadbeef",
        );
    }

    #[test]
    fn build_command_for_each_selector_kind() {
        let id: SessionId = "deadbeef".parse().unwrap();
        assert_eq!(
            build_seeit_command(id, &Selector::Stream("Nexus".into())),
            "seeit --session deadbeef --stream Nexus",
        );
        assert_eq!(
            build_seeit_command(id, &Selector::Tab("Nexus".into())),
            "seeit --session deadbeef --tab Nexus",
        );
        assert_eq!(
            build_seeit_command(id, &Selector::Bookmark("panic".into())),
            "seeit --session deadbeef --bookmark panic",
        );
    }

    #[test]
    fn build_command_shell_quotes_names_with_spaces() {
        let id: SessionId = "deadbeef".parse().unwrap();
        let cmd = build_seeit_command(id, &Selector::Tab("Tab 1".into()));
        // `shlex::try_quote` chooses single-quotes for ASCII-with-space
        // input.  We assert the structure rather than the exact form so
        // future shlex tweaks (e.g. choosing double-quotes) don't break
        // the test.
        assert!(
            cmd.ends_with("--tab 'Tab 1'") || cmd.ends_with("--tab \"Tab 1\""),
            "expected shell-quoted tab name, got: {cmd}"
        );
    }

    #[test]
    fn build_command_round_trips_through_a_shell_split() {
        // The whole point of building this string is feeding it back
        // to a shell.  Verify that splitting it via shlex yields the
        // exact arg vector that recreates the selector.
        let id: SessionId = "12345678".parse().unwrap();
        let cmd = build_seeit_command(id, &Selector::Tab("Tab 1".into()));
        let argv = shlex::split(&cmd).expect("valid shell quoting");
        assert_eq!(
            argv,
            vec!["seeit", "--session", "12345678", "--tab", "Tab 1"],
        );
    }

    #[test]
    fn resolve_loads_from_store() {
        // End-to-end: save a session, then resolve through the store.
        let dir = tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let store = SessionStore::open_at(&store_dir).unwrap();
        let session = fixture_session(dir.path());
        let id = session.id;
        store.save(id, &session).unwrap();

        let t = resolve(&store, id, &Selector::WholeSession).unwrap();
        assert_eq!(t.sources.len(), 2);
    }
}
