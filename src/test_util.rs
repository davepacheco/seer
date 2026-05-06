// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers shared across the crate's unit tests.
//!
//! Two purposes:
//!
//! - `TestDir`: a temporary directory that is preserved on failure so the
//!   contents can be inspected, and removed only when the test calls
//!   [`TestDir::cleanup`] explicitly.
//! - `append_bunyan` / `append_raw`: write fixture log files using slog's
//!   bunyan formatter (so test inputs match what bunyan-emitting code
//!   actually produces) plus an escape hatch for deliberately malformed
//!   lines.

use camino::Utf8Path;
use camino_tempfile::Utf8TempDir;
use slog::{Drain, Logger, o};
use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

/// Test temporary directory that is preserved on drop unless
/// [`TestDir::cleanup`] is called.
///
/// The convention: at the end of a successful test, call
/// `dir.cleanup()`.  Tests that panic or return early leave the
/// directory behind, with its path noted on stderr.
pub(crate) struct TestDir {
    inner: Option<Utf8TempDir>,
}

impl TestDir {
    pub(crate) fn new() -> Self {
        let inner =
            Utf8TempDir::new().expect("create test temp dir");
        Self { inner: Some(inner) }
    }

    pub(crate) fn path(&self) -> &Utf8Path {
        self.inner
            .as_ref()
            .expect("test temp dir already cleaned up")
            .path()
    }

    /// Removes the temporary directory.  Call only on the success path
    /// of a test.
    pub(crate) fn cleanup(mut self) {
        let inner =
            self.inner.take().expect("test temp dir already cleaned up");
        if let Err(e) = inner.close() {
            panic!("cleanup of test temp dir failed: {e}");
        }
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

/// Opens `path` for append (creating it if needed) and runs `body` with
/// a slog [`Logger`] that writes bunyan-formatted records to it.
///
/// `name` becomes the bunyan `name` field on every record written by
/// `body`.  It is `&'static str` because that is what
/// [`slog_bunyan::with_name`] requires; in practice tests pass string
/// literals.
pub(crate) fn append_bunyan<F>(
    path: &Utf8Path,
    name: &'static str,
    body: F,
) where
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

/// Appends an arbitrary raw line to `path`.
///
/// Useful for inserting deliberately malformed lines into otherwise
/// well-formed bunyan files so tests can exercise per-line error
/// handling.
pub(crate) fn append_raw(path: &Utf8Path, line: &str) {
    let mut f = File::options()
        .create(true)
        .append(true)
        .open(path)
        .expect("open log file for append");
    writeln!(f, "{line}").expect("write raw line");
}
