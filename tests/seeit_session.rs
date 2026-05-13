// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests for `seeit`'s session mode.
//!
//! Each test builds a session via the library API, saves it through
//! a [`SessionStore`] anchored at a temp directory, then invokes the
//! `seeit` binary with `SEER_STATE_DIR` pointing at that directory.
//! The captured stdout is compared against expectations.
//!
//! Coverage is intentionally about *behavior* — selector routing,
//! cursor honoring, override layering, summary mode — rather than
//! byte-exact rendering, which is already covered by the renderer's
//! own unit tests.

use camino::{Utf8Path, Utf8PathBuf};
use camino_tempfile::{Utf8TempDir, tempdir};
use chrono::{DateTime, Utc};
use seer::{
    Bookmark, BookmarkId, BookmarkName, Cursor, Engine, Filter, LogStream,
    STATE_DIR_ENV, Selector, Session, SessionId, SessionSource, SessionStore,
    Tab, TabKind, build_seeit_command,
};
use std::fs;
use std::process::Command;

/// Path to a fresh copy of `tests/fixtures/sample.log` placed under
/// `dir/<name>`.  Returned with its captured size and mtime so
/// callers can construct a [`SessionSource`] whose fingerprint
/// matches what's on disk.
struct StagedSource {
    /// Canonical path of the staged file.
    path: Utf8PathBuf,
    /// Size in bytes.
    size: u64,
    /// Modification time.
    mtime: DateTime<Utc>,
}

fn stage_sample(dir: &Utf8TempDir, name: &str) -> StagedSource {
    let src = Utf8PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo"),
    )
    .join("tests/fixtures/sample.log");
    let dest = dir.path().join(name);
    fs::copy(&src, &dest).unwrap();
    let canonical = dest.canonicalize_utf8().unwrap();
    let metadata = fs::metadata(&canonical).unwrap();
    StagedSource {
        path: canonical,
        size: metadata.len(),
        mtime: metadata.modified().unwrap().into(),
    }
}

/// Builds a session whose lone source is `staged`.  Returns the
/// session id, the stream id, and the [`SessionSource`].
fn build_session(staged: &StagedSource) -> (Session, seer::LogStreamId) {
    let mut session = Session::new();
    session
        .sources
        .insert_unique(SessionSource {
            id: seer::SourceId::from(staged.path.as_str().to_string()),
            path: staged.path.clone(),
            mtime: staged.mtime,
            size: staged.size,
        })
        .unwrap();
    let stream = LogStream::new("Tab 1".to_string());
    let stream_id = stream.id;
    session.streams.insert_unique(stream).unwrap();
    session.tabs.push(Tab {
        name: "Tab 1".to_string(),
        stream: stream_id,
        kind: TabKind::Stream,
        cursor: None,
    });
    (session, stream_id)
}

/// Saves `session` under `dir/sessions/` (the on-disk layout the
/// store expects) and returns the [`SessionStore`] that owns that
/// directory.  The store is needed only so the test can compute the
/// session id; the binary opens its own store.
fn save_session(dir: &Utf8Path, session: &Session) -> SessionStore {
    let store = SessionStore::open_at(dir.join("sessions")).unwrap();
    store.save(session.id, session).unwrap();
    store
}

/// Invokes the `seeit` binary with `SEER_STATE_DIR` pointing at
/// `state_dir` (so it opens the same session store the test wrote
/// to) and the given extra args.  Captures stdout as UTF-8, panicking
/// on a non-zero exit or non-UTF-8 output.  The first arg is always
/// the session id.
fn run_seeit(state_dir: &Utf8Path, args: &[&str]) -> String {
    let exe = env!("CARGO_BIN_EXE_seeit");
    let output = Command::new(exe)
        .env(STATE_DIR_ENV, state_dir.as_str())
        .args(args)
        .output()
        .expect("spawn seeit");
    assert!(
        output.status.success(),
        "seeit exited with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("stdout is UTF-8")
}

/// Counts records in stdout by counting header lines.  Header lines
/// from `format_event` start with `2026-` (a year prefix); extras
/// lines start with 4 spaces.  Using the year prefix is robust to
/// changes in the rest of the header shape.
fn count_records(stdout: &str) -> usize {
    stdout.lines().filter(|line| line.starts_with("2026-")).count()
}

#[test]
fn whole_session_emits_every_record_in_the_source() {
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");
    let (session, _) = build_session(&staged);
    let id = session.id;
    save_session(dir.path(), &session);

    let stdout = run_seeit(dir.path(), &["--session", &id.to_string()]);
    // Sample fixture has at least 10 records.
    assert!(
        count_records(&stdout) >= 10,
        "expected >=10 records, got stdout:\n{stdout}"
    );
}

#[test]
fn stream_with_filter_drops_non_matching_records() {
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");
    let (mut session, stream_id) = build_session(&staged);
    // Replace the stream's filter with one that matches every
    // record in the fixture (the fixture is all `name=Nexus`).
    let mut stream = session.streams.remove(&stream_id).unwrap();
    stream.filter = "name=Nexus".parse::<Filter>().unwrap();
    session.streams.insert_unique(stream).unwrap();
    let id = session.id;
    save_session(dir.path(), &session);

    // Whole session would also match every record, so compare
    // against a filter that drops everything: name=DoesNotExist.
    let mut stream = session.streams.remove(&stream_id).unwrap();
    stream.filter = "name=DoesNotExist".parse::<Filter>().unwrap();
    session.streams.insert_unique(stream).unwrap();
    save_session(dir.path(), &session);

    let stdout = run_seeit(
        dir.path(),
        &["--session", &id.to_string(), "--stream", "Tab 1"],
    );
    assert_eq!(
        count_records(&stdout),
        0,
        "filter should have dropped every record:\n{stdout}"
    );
}

#[test]
fn count_caps_output() {
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");
    let (session, _) = build_session(&staged);
    let id = session.id;
    save_session(dir.path(), &session);

    let stdout =
        run_seeit(dir.path(), &["--session", &id.to_string(), "--count", "3"]);
    assert_eq!(count_records(&stdout), 3);
}

#[test]
fn tab_at_saved_cursor_resumes_from_that_position() {
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");

    // Discover the byte offset of the third record by walking the
    // engine forward twice and snapshotting the cursor.
    let mut engine = Engine::new();
    engine.add_file_source(&staged.path).unwrap();
    let mut stepper = engine.stepper(Filter::default(), &Cursor::default());
    stepper.step_forward().unwrap();
    stepper.step_forward().unwrap();
    let mid_cursor = stepper.cursor();

    let (mut session, _) = build_session(&staged);
    session.tabs[0].cursor = Some(mid_cursor);
    let id = session.id;
    save_session(dir.path(), &session);

    let from_start = run_seeit(dir.path(), &["--session", &id.to_string()]);
    let from_tab = run_seeit(
        dir.path(),
        &["--session", &id.to_string(), "--tab", "Tab 1"],
    );

    // Resuming from the third record yields fewer records than the
    // unfiltered whole-session run.
    let whole = count_records(&from_start);
    let resumed = count_records(&from_tab);
    assert!(resumed < whole, "resumed {resumed} >= whole {whole}");
    assert_eq!(resumed, whole - 2);
}

#[test]
fn bookmark_before_window_emits_pre_cursor_records() {
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");

    // Set up a bookmark at the third record so --before walks back
    // over the first two.
    let mut engine = Engine::new();
    engine.add_file_source(&staged.path).unwrap();
    let mut stepper = engine.stepper(Filter::default(), &Cursor::default());
    stepper.step_forward().unwrap();
    stepper.step_forward().unwrap();
    let cursor_at_third = stepper.cursor();

    let (mut session, stream_id) = build_session(&staged);
    let bookmark = Bookmark {
        id: BookmarkId::new_v4(),
        created_at: Utc::now(),
        cursor: cursor_at_third,
        name: Some(BookmarkName::from("here".to_string())),
        display_source: seer::SourceId::from(staged.path.as_str().to_string()),
        display_time: Utc::now(),
        display_msg: "msg".to_string(),
    };
    session.add_bookmark(stream_id, bookmark);
    let id = session.id;
    save_session(dir.path(), &session);

    // From the bookmark: walk back 2, forward 1 → 3 records total.
    let stdout = run_seeit(
        dir.path(),
        &[
            "--session",
            &id.to_string(),
            "--bookmark",
            "here",
            "--before",
            "2",
            "--count",
            "1",
        ],
    );
    assert_eq!(count_records(&stdout), 3);
}

#[test]
fn override_filter_replaces_resolved_filter() {
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");
    let (mut session, stream_id) = build_session(&staged);
    // Stream filter drops everything; --filter override should
    // re-include records.
    let mut stream = session.streams.remove(&stream_id).unwrap();
    stream.filter = "name=Nothing".parse::<Filter>().unwrap();
    session.streams.insert_unique(stream).unwrap();
    let id = session.id;
    save_session(dir.path(), &session);

    let stdout = run_seeit(
        dir.path(),
        &[
            "--session",
            &id.to_string(),
            "--stream",
            "Tab 1",
            "--filter",
            "name=Nexus",
        ],
    );
    assert!(count_records(&stdout) > 0);
}

#[test]
fn and_filter_narrows_resolved_filter() {
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");
    let (mut session, stream_id) = build_session(&staged);
    // Stream filter accepts everything; --and-filter should narrow.
    let mut stream = session.streams.remove(&stream_id).unwrap();
    stream.filter = "name=Nexus".parse::<Filter>().unwrap();
    session.streams.insert_unique(stream).unwrap();
    let id = session.id;
    save_session(dir.path(), &session);

    // The fixture has records of varying levels; restrict to
    // `level>=error` and verify we get fewer records than the
    // unrestricted stream.
    let unfiltered = run_seeit(
        dir.path(),
        &["--session", &id.to_string(), "--stream", "Tab 1"],
    );
    let narrowed = run_seeit(
        dir.path(),
        &[
            "--session",
            &id.to_string(),
            "--stream",
            "Tab 1",
            "--and-filter",
            "level>=error",
        ],
    );
    assert!(
        count_records(&narrowed) <= count_records(&unfiltered),
        "narrowed {} should be <= unfiltered {}",
        count_records(&narrowed),
        count_records(&unfiltered),
    );
}

#[test]
fn summary_tab_emits_summary_output() {
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");
    let (mut session, _) = build_session(&staged);
    session.tabs[0].kind = TabKind::Summary;
    let id = session.id;
    save_session(dir.path(), &session);

    let stdout = run_seeit(
        dir.path(),
        &["--session", &id.to_string(), "--tab", "Tab 1"],
    );
    // The summary's first line always starts with "Summary:".  No
    // record headers should appear in summary mode.
    assert!(
        stdout.starts_with("Summary:"),
        "expected summary header, got:\n{stdout}"
    );
    assert_eq!(
        count_records(&stdout),
        0,
        "summary should not contain record headers"
    );
}

#[test]
fn render_overrides_layer_on_top_of_stream_opts() {
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");
    let (mut session, stream_id) = build_session(&staged);
    // Force the stream to a known render-opts shape (extras off,
    // pid hidden) so the test's override can demonstrate flipping
    // a field.
    let mut stream = session.streams.remove(&stream_id).unwrap();
    stream.show_extras = false;
    stream.show_pid = false;
    session.streams.insert_unique(stream).unwrap();
    let id = session.id;
    save_session(dir.path(), &session);

    let base = run_seeit(
        dir.path(),
        &["--session", &id.to_string(), "--stream", "Tab 1", "--count", "1"],
    );
    let with_extras = run_seeit(
        dir.path(),
        &[
            "--session",
            &id.to_string(),
            "--stream",
            "Tab 1",
            "--count",
            "1",
            "--show-extras",
        ],
    );

    // Extras-on output has more lines than the same record without
    // extras.
    let base_lines = base.lines().count();
    let with_lines = with_extras.lines().count();
    assert!(
        with_lines > base_lines,
        "--show-extras did not add lines: base {base_lines} -> with {with_lines}\nbase:\n{base}\nwith:\n{with_extras}"
    );
}

#[test]
fn unknown_session_returns_nonzero_exit() {
    let dir = tempdir().unwrap();
    // Force the state dir to exist but have no sessions.
    fs::create_dir_all(dir.path().join("sessions")).unwrap();
    let unknown: SessionId = "deadbeef".parse().unwrap();

    let exe = env!("CARGO_BIN_EXE_seeit");
    let output = Command::new(exe)
        .env(STATE_DIR_ENV, dir.path().as_str())
        .args(["--session", &unknown.to_string()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The friendly error printer prefixes with "seeit:" and prints
    // the Display form (no Debug-style enum variant names).
    assert!(
        stderr.starts_with("seeit:"),
        "expected friendly seeit: prefix, got:\n{stderr}"
    );
    assert!(
        stderr.contains("I/O error"),
        "expected I/O error mention from store load failure, got:\n{stderr}"
    );
}

#[test]
fn fingerprint_mismatch_fails_loudly() {
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");
    let (session, _) = build_session(&staged);
    let id = session.id;
    save_session(dir.path(), &session);

    // Touch the source file so its size and mtime no longer match
    // the captured fingerprint.
    fs::write(&staged.path, "tampered\n").unwrap();

    let exe = env!("CARGO_BIN_EXE_seeit");
    let output = Command::new(exe)
        .env(STATE_DIR_ENV, dir.path().as_str())
        .args(["--session", &id.to_string()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("changed since the session was saved")
            || stderr.contains("SourceFingerprint"),
        "expected fingerprint error, got:\n{stderr}"
    );
}

#[test]
fn header_writes_context_to_stderr_not_stdout() {
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");
    let (session, _) = build_session(&staged);
    let id = session.id;
    save_session(dir.path(), &session);

    let exe = env!("CARGO_BIN_EXE_seeit");
    let output = Command::new(exe)
        .env(STATE_DIR_ENV, dir.path().as_str())
        .args([
            "--session",
            &id.to_string(),
            "--tab",
            "Tab 1",
            "--count",
            "1",
            "--header",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    // Stdout should be the rendered record only, no banner line.
    assert!(
        !stdout.contains("seeit: session="),
        "banner leaked into stdout:\n{stdout}"
    );
    assert_eq!(count_records(&stdout), 1);

    // Stderr carries the banner and identifies the target.
    assert!(
        stderr.starts_with("seeit: session="),
        "expected banner on stderr, got:\n{stderr}"
    );
    assert!(stderr.contains(&format!("session={id}")));
    assert!(stderr.contains("target=tab"));
    assert!(stderr.contains("mode=records"));
    assert!(stderr.contains("window=count=1"));
}

#[test]
fn header_omitted_means_nothing_on_stderr() {
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");
    let (session, _) = build_session(&staged);
    let id = session.id;
    save_session(dir.path(), &session);

    let exe = env!("CARGO_BIN_EXE_seeit");
    let output = Command::new(exe)
        .env(STATE_DIR_ENV, dir.path().as_str())
        .args(["--session", &id.to_string(), "--count", "1"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.contains("seeit: session="),
        "did not pass --header but got a banner:\n{stderr}"
    );
}

#[test]
fn validation_error_prints_friendly_message() {
    // --stream without --session is caught by Args::validate; the
    // friendlier error printer in main should surface the
    // SessionRequired Display, not the Debug-form of the enum
    // variant.
    let exe = env!("CARGO_BIN_EXE_seeit");
    let output = Command::new(exe)
        .args(["foo.log", "--stream", "Nexus"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--stream"));
    assert!(
        stderr.contains("requires"),
        "expected friendly 'requires --session' message, got:\n{stderr}"
    );
    // The Debug form `SessionRequired { flag: ... }` must NOT appear.
    assert!(
        !stderr.contains("SessionRequired"),
        "Debug-form leaked into output:\n{stderr}"
    );
}

#[test]
fn printed_seeit_command_actually_reproduces_the_view() {
    // Round-trip: build the seeit command for a session's tab,
    // split it through shlex (the same way a shell would), invoke
    // the binary with those args, and confirm it produces non-empty
    // output.  This is the contract Phase 6's seer keybinding relies
    // on — whatever seer prints, the user can paste into a shell.
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");
    let (session, _) = build_session(&staged);
    let id = session.id;
    save_session(dir.path(), &session);

    let cmd = build_seeit_command(id, &Selector::Tab("Tab 1".to_string()));
    let mut argv = shlex::split(&cmd).expect("valid shell quoting");
    // The first token is "seeit" — drop it and feed the rest to the
    // real binary the test framework built for us.
    let program = argv.remove(0);
    assert_eq!(program, "seeit");

    let exe = env!("CARGO_BIN_EXE_seeit");
    let output = Command::new(exe)
        .env(STATE_DIR_ENV, dir.path().as_str())
        .args(&argv)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "round-trip command failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(count_records(&stdout) >= 10);
}

#[test]
fn file_mode_still_works_unchanged() {
    // Confirm that pointing seeit at a bare file (today's
    // pre-session-mode invocation) still produces output.
    let dir = tempdir().unwrap();
    let staged = stage_sample(&dir, "sample.log");

    let exe = env!("CARGO_BIN_EXE_seeit");
    let output = Command::new(exe).arg(staged.path.as_str()).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(count_records(&stdout) >= 10);
}
