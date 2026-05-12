// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fixture helpers for unit tests, integration tests, and benchmarks.
//!
//! Three categories of helper live here:
//!
//! - [`TestDir`]: a temporary directory that is preserved on failure so
//!   its contents can be inspected, and removed only when the caller
//!   invokes [`TestDir::cleanup`].
//! - [`append_bunyan_at`] / [`append_raw`]: write a single line to a
//!   fixture file, with controlled timestamp or arbitrary content
//!   (useful for deliberately malformed lines).  A `slog`-based
//!   [`append_bunyan`] is also available, but only to crate-internal
//!   unit tests because `slog` is a dev-dependency.
//! - [`gen_single_source`] / [`gen_multi_source`] /
//!   [`gen_with_parse_errors`]: bulk emitters that write hundreds or
//!   thousands of records, used by scale tests under `tests/` and by
//!   benches under `benches/`.
//!
//! The module is exposed publicly only when compiling under
//! `#[cfg(test)]` or with the `test-fixtures` feature enabled (see
//! `lib.rs`).  Consumers outside the crate enable it via
//! `required-features = ["test-fixtures"]` on the relevant `[[test]]`
//! or `[[bench]]` entry in `Cargo.toml`.

use camino::{Utf8Path, Utf8PathBuf};
use camino_tempfile::Utf8TempDir;
use chrono::{DateTime, Duration, TimeZone, Utc};
use std::fs::File;
use std::io::Write;

#[cfg(test)]
use slog::{Drain, Logger, o};
#[cfg(test)]
use std::sync::Mutex;

/// Builds a [`DateTime<Utc>`] from epoch seconds.
///
/// Used across merge / source / engine tests to anchor fixture records
/// on predictable timestamps so that ordering can be asserted exactly.
pub fn t(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).single().expect("valid timestamp")
}

/// Test temporary directory that is preserved on drop unless
/// [`TestDir::cleanup`] is called.
///
/// Convention: at the end of a successful test, call `dir.cleanup()`.
/// Tests that panic or return early leave the directory behind, with
/// its path noted on stderr so the author can inspect it.
pub struct TestDir {
    inner: Option<Utf8TempDir>,
}

impl TestDir {
    pub fn new() -> Self {
        let inner = Utf8TempDir::new().expect("create test temp dir");
        Self { inner: Some(inner) }
    }

    pub fn path(&self) -> &Utf8Path {
        self.inner.as_ref().expect("test temp dir already cleaned up").path()
    }

    /// Removes the temporary directory.  Call only on the success path
    /// of a test.
    pub fn cleanup(mut self) {
        let inner =
            self.inner.take().expect("test temp dir already cleaned up");
        if let Err(e) = inner.close() {
            panic!("cleanup of test temp dir failed: {e}");
        }
    }
}

impl Default for TestDir {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            // `cleanup()` was not called; preserve the directory so the
            // test author can inspect it.
            let path = inner.keep();
            eprintln!(
                "preserving test temp dir (cleanup() not called): {path}"
            );
        }
    }
}

/// Opens `path` for append (creating it if needed) and runs `body`
/// with a slog [`Logger`] that writes bunyan-formatted records to it.
///
/// `name` becomes the bunyan `name` field on every record written by
/// `body`.  It is `&'static str` because that is what
/// [`slog_bunyan::with_name`] requires; in practice tests pass string
/// literals.
///
/// Available only to crate-internal unit tests; `slog` is a
/// dev-dependency and is not part of the `test-fixtures` feature
/// surface.
#[cfg(test)]
pub fn append_bunyan<F>(path: &Utf8Path, name: &'static str, body: F)
where
    F: FnOnce(&Logger),
{
    let file = File::options()
        .create(true)
        .append(true)
        .open(path)
        .expect("open log file for append");
    let drain = slog_bunyan::with_name(name, file).build().fuse();
    let drain = Mutex::new(drain).fuse();
    let log = Logger::root(drain, o!());
    body(&log);
}

/// Appends a single bunyan-formatted line to `path` with a chosen
/// timestamp and message.
///
/// Other bunyan-core fields take fixed values (`level=info`,
/// `hostname="test-host"`, `pid=42`, `v=0`); tests that need to vary
/// those should use [`gen_single_source`] with a customised
/// [`GenOpts`].
pub fn append_bunyan_at(
    path: &Utf8Path,
    name: &str,
    time: DateTime<Utc>,
    msg: &str,
) {
    let line = serde_json::json!({
        "v": 0,
        "level": 30,
        "name": name,
        "hostname": "test-host",
        "pid": 42,
        "time": time.to_rfc3339(),
        "msg": msg,
    });
    append_raw(path, &line.to_string());
}

/// Appends an arbitrary raw line to `path`.
///
/// Useful for inserting deliberately malformed lines into otherwise
/// well-formed bunyan files so tests can exercise per-line error
/// handling.
pub fn append_raw(path: &Utf8Path, line: &str) {
    let mut f = File::options()
        .create(true)
        .append(true)
        .open(path)
        .expect("open log file for append");
    writeln!(f, "{line}").expect("write raw line");
}

/// Options controlling bulk-fixture emission.
///
/// All fields are public so callers can tweak individual aspects
/// without recreating the whole struct, e.g.
/// `GenOpts { extras_every: 50, ..GenOpts::default() }`.
#[derive(Clone, Debug)]
pub struct GenOpts {
    /// Timestamp of the first record.
    pub base_time: DateTime<Utc>,
    /// Spacing between consecutive records.
    pub step: Duration,
    /// Numeric bunyan level codes rotated across records.  (10 = trace,
    /// 20 = debug, 30 = info, 40 = warn, 50 = error.)
    pub levels: Vec<u8>,
    /// Message strings rotated across records.
    pub message_templates: Vec<String>,
    /// Every `extras_every`-th record carries additional top-level
    /// fields so summary tests have something to histogram.  Zero
    /// disables extras entirely.
    pub extras_every: usize,
    /// Value of the bunyan `hostname` field.
    pub hostname: String,
    /// Value of the bunyan `pid` field.
    pub pid: u32,
}

impl Default for GenOpts {
    fn default() -> Self {
        Self {
            base_time: Utc
                .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                .single()
                .expect("valid base time"),
            step: Duration::milliseconds(100),
            levels: vec![20, 30, 30, 30, 40, 50],
            message_templates: vec![
                "request started".to_string(),
                "request completed".to_string(),
                "cache miss".to_string(),
                "db query executed".to_string(),
                "task scheduled".to_string(),
            ],
            extras_every: 0,
            hostname: "test-host".to_string(),
            pid: 42,
        }
    }
}

/// Writes `count` bunyan-formatted records to `path`.
///
/// Timestamps are `opts.base_time + i * opts.step`, monotonically
/// increasing.  Level and message rotate through their respective
/// lists.  When `opts.extras_every > 0`, every Nth record (starting at
/// index 0) carries additional `req_id`, `user`, and `component`
/// fields so summary tests have something to histogram.
pub fn gen_single_source(
    path: &Utf8Path,
    name: &str,
    count: usize,
    opts: &GenOpts,
) {
    let mut f = File::options()
        .create(true)
        .append(true)
        .open(path)
        .expect("open log file for append");
    for i in 0..count {
        let i_i32 =
            i32::try_from(i).expect("record index fits in i32 for fixtures");
        let time = opts.base_time + opts.step * i_i32;
        let level = opts.levels[i % opts.levels.len()];
        let msg = &opts.message_templates[i % opts.message_templates.len()];
        let mut record = serde_json::json!({
            "v": 0,
            "level": level,
            "name": name,
            "hostname": opts.hostname,
            "pid": opts.pid,
            "time": time
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "msg": msg,
        });
        if opts.extras_every > 0 && i % opts.extras_every == 0 {
            let obj =
                record.as_object_mut().expect("record is a JSON object");
            obj.insert(
                "req_id".to_string(),
                serde_json::Value::String(format!("req-{i}")),
            );
            obj.insert(
                "user".to_string(),
                serde_json::Value::String(format!("u{}", i % 100)),
            );
            obj.insert(
                "component".to_string(),
                serde_json::Value::String("test".to_string()),
            );
        }
        writeln!(f, "{record}").expect("write line");
    }
}

/// Writes one fixture file per source name, with interleaved
/// timestamps so a merged view has real ordering work to do.
///
/// Returns the paths in the same order as `source_names`.  Each source
/// gets `count_per_source` records spaced by `GenOpts::default()`'s
/// step; sources are staggered by `step / source_names.len()` so a
/// merged walk alternates between them at every record.
pub fn gen_multi_source(
    dir: &Utf8Path,
    count_per_source: usize,
    source_names: &[&str],
) -> Vec<Utf8PathBuf> {
    let base = GenOpts::default();
    let n = i32::try_from(source_names.len())
        .expect("source count fits in i32 for fixtures");
    let stagger = if n > 0 { base.step / n } else { Duration::zero() };
    let mut paths = Vec::with_capacity(source_names.len());
    for (idx, name) in source_names.iter().enumerate() {
        let path = dir.join(format!("{name}.log"));
        let idx_i32 = i32::try_from(idx)
            .expect("source index fits in i32 for fixtures");
        let opts = GenOpts {
            base_time: base.base_time + stagger * idx_i32,
            ..base.clone()
        };
        gen_single_source(&path, name, count_per_source, &opts);
        paths.push(path);
    }
    paths
}

/// Writes `count` records to `path`, where every `error_every`-th
/// line (`error_every > 0`) is intentionally malformed and the rest
/// are valid bunyan.
///
/// Useful for testing per-line error handling on files large enough to
/// exercise the chunked-read paths.
pub fn gen_with_parse_errors(
    path: &Utf8Path,
    count: usize,
    error_every: usize,
) {
    let opts = GenOpts::default();
    let mut f = File::options()
        .create(true)
        .append(true)
        .open(path)
        .expect("open log file for append");
    for i in 0..count {
        if error_every > 0 && i % error_every == 0 {
            writeln!(f, "not a valid bunyan record at index {i}")
                .expect("write malformed line");
        } else {
            let i_i32 = i32::try_from(i)
                .expect("record index fits in i32 for fixtures");
            let time = opts.base_time + opts.step * i_i32;
            let record = serde_json::json!({
                "v": 0,
                "level": 30,
                "name": "test",
                "hostname": opts.hostname,
                "pid": opts.pid,
                "time": time
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "msg": "valid record",
            });
            writeln!(f, "{record}").expect("write record");
        }
    }
}
