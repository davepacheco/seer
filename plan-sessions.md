# Plan: persistent sessions

## For Claude: recording progress

This file is the source of truth for where we are in the work.  The
user may pick up the project on any day and just say "take the next
step" — that's enough only if this file accurately reflects what's
done and what's next.  So:

- When **starting** a phase, change its status in the "Phasing"
  section from `[ ]` to `[~]` (in progress).
- When **finishing** a phase, change `[~]` to `[x]` and add a short
  note underneath it: the commit hash (if committed), or a one-line
  summary of what landed and where (file paths, function names).  If
  the implementation diverged from what the phase originally said,
  say so — the next reader needs to trust the file.
- If you discover something mid-phase that affects later phases
  (a renamed type, a deferred sub-task, a new test gap), update
  the relevant later phase right away rather than leaving it
  inconsistent.
- The **Current status** line just below tracks the next phase to
  pick up.  Update it whenever a phase moves to `[x]`.
- If a phase turns out to be wrong or no longer makes sense, edit
  it.  This file is not sacred — keeping it accurate matters more
  than keeping it stable.

## Current status

Phase 3 complete.  Next phase: **4. Save policy.**

## Goal

Let a user exit `seer`, re-open it later on the same log files, and pick up
where they left off — same tabs, filters, cursors, and bookmarks.  Resuming
should be explicit (the user chooses), not magic.

## On-disk layout

Sessions live under the XDG state directory:

```
$XDG_STATE_HOME/seer/sessions/<session_id>.json
```

`$XDG_STATE_HOME` defaults to `~/.local/state` on Linux.  Use the
[`etcetera`](https://crates.io/crates/etcetera) crate (or `directories`,
whichever fits cleaner with `camino`) to resolve it.  An environment
override, `SEER_STATE_DIR`, lets tests redirect the directory without
touching `$HOME`.

`<session_id>` is an 8-character lowercase hex string drawn from a UUID's
first four bytes.  This is short enough to type after `--resume` and long
enough that collisions in a single user's session dir are vanishingly
unlikely.  Collisions are still checked at create time; on collision, try
again.

The file is JSON (pretty-printed for diffability).  All session writes go
through a tmp-file-plus-rename so a crash mid-write can't leave a
truncated file behind.

## Session schema

The on-disk `Session` extends what's already in `src/session.rs` with the
fields needed to find and label saved sessions:

```rust
pub struct Session {
    /// On-disk schema version.  See "Schema versioning" below.
    pub version: u32,

    /// Short hex id; also the filename stem.
    pub id: SessionId,

    /// Sources that this session was opened against.  Matching at
    /// startup is by canonical path for now; fingerprinting can be
    /// added later without restructuring this.
    pub sources: Vec<SessionSource>,

    /// When the session was first created.
    pub created_at: DateTime<Utc>,

    /// When the saver last flushed to disk.  Useful for the resume
    /// dialog and for "stale lock" diagnostics.
    pub last_saved_at: DateTime<Utc>,

    /// PID of the `seer` process that most recently saved this
    /// session.  Recorded so a future concurrent-access check has
    /// something to surface; not consulted for correctness yet.
    pub last_pid: u32,

    pub tabs: Vec<Tab>,
    pub streams: IdOrdMap<LogStream>,
    pub user_bookmarks: BTreeMap<LogStreamId, Vec<Bookmark>>,
}

pub struct SessionSource {
    pub id: SourceId,
    /// Canonical path captured at open time.
    pub path: Utf8PathBuf,
    /// File modification time at the moment we recorded this source.
    /// Compared against the live file's mtime at resume time; a
    /// mismatch means the file has changed since the session was
    /// saved and we should warn the user (and invalidate any cached
    /// parse state for that source).
    pub mtime: DateTime<Utc>,
    /// File size in bytes at the moment we recorded this source.
    /// Stored alongside `mtime` because both are essentially free to
    /// capture and the pair is a more robust fingerprint than either
    /// alone — useful both for change detection and as the seed for
    /// any future path-independent matching.
    pub size: u64,
}
```

Note: no `#[serde(default)]` on the new fields.  There are no live
session files to be backwards-compatible with, so we start clean.

### Schema versioning

1. `Session::version` is persisted to disk and bumped whenever the
   serialized shape changes in a non-trivial way.
2. Derive `schemars::JsonSchema` (`schemars` ^0.8) on `Session` and every
   type reachable through it.
3. Add an `expectorate`-based test that generates the schema and
   compares it to a checked-in fixture (`tests/fixtures/session.schema.json`).
   Any accidental schema change fails the test and prompts the author to
   either revert or bump `version` and refresh the fixture with
   `EXPECTORATE=overwrite`.

This gives us a tripwire without committing to a migration framework
before we know we need one.

## Session discovery

At `seer` startup, when the user supplies source paths:

1. Canonicalize each path the user gave on the command line.
2. Scan `sessions/` and parse every `.json` file (we expect dozens at
   most, not thousands; a manifest can wait).
3. Classify each candidate session by source overlap with the supplied
   paths:
   - *exact*: same set
   - *superset*: session contains all user paths plus extras
   - *overlap*: at least one shared path
   - *none*: skip

The dialog shows up to ~10 candidates ordered by `last_saved_at`
descending, exact matches first, with an option to "show all" if there
are more.

## Saver

Saving happens **synchronously on the TUI thread.** No background
thread, no channel, no queue.  The serialized session is tens of KB
and a local write is ~1–5 ms — well below any frame-budget threshold.

The TUI tracks two pieces of state alongside the session:

- `dirty: bool` — set whenever a session-affecting mutation happens.
- `last_saved_at: Instant` — when the saver last successfully wrote.

The save policy then folds into the event loop:

- **Low-cadence, high-value mutations** save inline, right away:
  - bookmark create/rename/delete
  - tab open/close
  - filter changes (named or unnamed)
  - field show/hide

- **High-cadence, low-value mutations** just set `dirty`:
  - cursor scrolling
  - viewport resize

  At the top of each frame (or each input event, whichever is more
  convenient), the loop checks `dirty && last_saved_at.elapsed() >=
  10s` and, if true, saves.  This is the debounce, expressed as a
  natural-language predicate instead of a timer thread.

- **On exit**, the TUI does a final save if `dirty`, then quits.

The whole `Session` is serialized each time.  In theory wasteful; in
practice these structs are small and serialization is cheap relative
to everything else the TUI does.  Worth revisiting if it ever shows
up in a flame graph.

### Trade-off accepted

A disk hiccup (slow NFS, fsync stall, full disk) will freeze the UI
for the duration of the write.  For local files on a healthy
filesystem this is rare and brief, and the simplicity of skipping
all the cross-thread machinery is worth the occasional stutter.  If
this ever bites in practice, the migration path is a single-slot
snapshot handed off to a saver thread — `Arc<Mutex<Option<Pending>>>`
plus a `Condvar`.  The save policy above stays unchanged; only the
write itself moves.

### Errors

If `save()` returns an error, the TUI surfaces it in the status bar
(or pops a dialog for the more serious ones — permission denied, no
space).  `dirty` stays set so the next opportunity tries again.
Failure to save on exit is reported on stdout alongside the resume
hint so the user isn't left thinking their session was persisted
when it wasn't.

## Startup flow

`seer` is invoked one of three ways:

### `seer FILE...`

1. Resolve paths, run discovery.
2. If candidates exist, show the resume dialog:
   - **Resume an existing session** — pick from the candidate list; each
     row shows the id, `last_saved_at`, tab count, source count, and
     whether the match is exact.
   - **Start a new saved session** — create a fresh session with one tab
     showing the merged view (today's no-args behavior), persist it.
   - **Start a transient session** — same, but no saver is attached and
     no file is ever written.
3. If no candidates exist, show the same dialog without the resume
   option.

### `seer --resume SESSION_ID`

1. Load the session by id.
2. Verify every source path still exists.  If any are missing, error out
   with a message naming them; offer no automatic relocation in v1.  (A
   `--relocate OLD=NEW` flag is a sound future addition.)
3. Open the TUI in the loaded state.

### `seer --list`

Print the saved-session table to stdout — `id`, `last_saved_at`, tab
count, source count, first source path (truncated).  Exit.

### Quit message

When the TUI exits normally, print to stdout:

```
session saved.  resume with: seer --resume <id>
```

Transient sessions print nothing.

## Concurrent access

Out of scope for this pass.  We record `last_pid` and `last_saved_at` in
every file so a future check can warn ("session was saved 30s ago by pid
12345, which is still running — open anyway?").  No OS-level lock yet.
The expected pattern — one user, one terminal — makes this an acceptable
risk; documenting it in the resume dialog's help text is enough for now.

## Module layout

```
src/
  session.rs         // existing: Session, Tab, Bookmark types
                     //   + new fields (id, sources, timestamps, pid)
                     //   + JsonSchema derives
  session_store.rs   // new: filesystem layer
                     //   - resolve_state_dir() / SEER_STATE_DIR
                     //   - SessionId type + filename mapping
                     //   - load(id), save_atomic(session), list_all()
                     //   - discovery: find_matches(paths) -> Vec<Match>
```

No dedicated saver module — the TUI calls `session_store::save_atomic`
directly under the policy described above.  `session_store` belongs
in the engine layer; the TUI invokes it but neither it nor `session`
knows about ratatui.

## CLI changes

`clap` already drives arg parsing.  Add:

- `--resume <SESSION_ID>` — mutually exclusive with positional files.
- `--list` — mutually exclusive with everything else, exits without
  opening the TUI.

## Tests

- Unit tests for `session_store`:
  - round-trip a populated `Session` through `save_atomic` / `load`.
  - `find_matches` returns the right classification for exact /
    superset / overlap / none.
  - `SEER_STATE_DIR` redirects writes (uses `camino-tempfile`).
  - tmp-file-plus-rename: simulate a crash mid-write (write a tmp file
    and leave it; verify load still returns the previous good state).

- Schema fixture test: generate `Session`'s schema with `schemars`,
  compare against `tests/fixtures/session.schema.json` via
  `expectorate`.

- Save-policy unit test: a small harness that drives the policy
  function with synthetic timestamps and `dirty`/mutation events,
  asserting inline saves for low-cadence events and debounced saves
  for the high-cadence ones.

- Integration test (in `tests/`): run a synthetic engine, mutate
  bookmarks and tabs, trigger the saves the policy would, reload,
  assert the new `Session` matches.

## New dependencies

- `etcetera` (or `directories`) — XDG resolution.
- `schemars` ^0.8 with the `derive` feature — schema generation.
- `expectorate` (dev-dep) — fixture diffing.

## Phasing

Each phase is independently mergeable.  Update the checkbox and add
a note when a phase moves to in-progress or done.

- [x] **1. Storage primitives.**  `SessionId`, `session_store`
  load/save/list, `SEER_STATE_DIR` resolution, atomic write, unit
  tests.  No integration with the TUI yet.
  - Landed in `src/session_store.rs`; re-exports added to
    `src/lib.rs`.  `SessionId` is an 8-char hex newtype (UUIDv4
    first four bytes) with `Display`/`FromStr`/`Serialize`/
    `Deserialize`.  `SessionStore::{open, open_at, load, save,
    list, path_for, sessions_dir}`.  `resolve_state_dir` takes an
    `env_lookup` closure so the env-var-override path is testable
    without mutating the process environment.  Added `etcetera =
    "0.11.0"` as a dependency.  13 unit tests in
    `session_store::tests` covering id round-trips, save/load,
    `list` filtering, the no-`.tmp`-leftovers atomicity contract,
    and the env-override fallback.

- [x] **2. Schema fields + fixture.**  Add `id`, `sources` (with
  `mtime` and `size`), `created_at`, `last_saved_at`, `last_pid`
  to `Session`; derive `JsonSchema`; land the `expectorate`
  fixture and the test that checks it.
  - `Session` in `src/session.rs` gained `id`, `sources`,
    `created_at`, `last_saved_at`, `last_pid`.  `SessionSource`
    added with `id`/`path`/`mtime`/`size`.  Removed
    `#[serde(default)]` from all `Session` fields (no live files
    to be compatible with) and dropped the `Default` impl;
    `Session::new()` mints a fresh id, timestamps, and pid.
    `CURRENT_SESSION_VERSION` bumped to 3.  `JsonSchema` derived
    across the full reachable graph (`Session`, `Tab`, `Bookmark`,
    `BookmarkName/Id`, `SessionSource`, `SourceId`, `ByteOffset`,
    `LogStream`, `LogStreamId`, `LogStreamPosition`, `Filter`,
    `Predicate`, `TimeOp`, `HostnameDisplay`, `Cursor`); manual
    impls for `SessionId` (8-char hex string) and `Level` (bunyan
    integer).  `Utf8PathBuf`/`Regex` use `#[schemars(with =
    "String")]`; `IdOrdMap<LogStream>` uses `#[schemars(with =
    "Vec<LogStream>")]`.  Schema fixture lives at
    `tests/fixtures/session.schema.json` and is diffed by
    `tests/session_schema.rs` via `expectorate` — refresh with
    `EXPECTORATE=overwrite cargo nextest run -p seer
    session_schema_matches_fixture`.  Added `schemars = "0.8.22"`
    (chrono+uuid1 features), `expectorate = "1.2.0"` dev-dep, and
    enabled `camino`'s `serde1` feature.  Retargeted two
    `legacy_session_*` tests in `src/bin/seer.rs` at `LogStream`
    deserialization directly, since the partial-Session JSON
    pattern they relied on no longer round-trips after the
    `#[serde(default)]` removal.  451 tests pass.

- [x] **3. Discovery.**  `find_matches(paths) -> Vec<Match>`,
  classified exact/superset/overlap.  Unit tests with a temp
  state dir.
  - Added `MatchKind` (Exact/Superset/Overlap, ordered for display)
    and `SessionMatch { kind, session }` in
    `src/session_store.rs`.  `SessionStore::find_matches(&[Utf8PathBuf])
    -> Vec<SessionMatch>` walks `list()`, deserializes each session,
    compares the source-path set against the caller's set via a
    `classify` helper using `BTreeSet<&Utf8Path>`, drops non-overlapping
    or empty-set sessions, and sorts by `kind` ascending then
    `last_saved_at` descending.  Parse errors on individual session
    files are silently skipped — unresumable anyway, and the file
    stays on disk for human investigation.  Re-exports added to
    `src/lib.rs`.  Eight new tests in `session_store::tests` cover
    exact / superset / overlap / disjoint / empty-user-paths /
    empty-store / empty-session-sources, the kind-then-recency sort,
    and the corrupt-file skip.  460 tests pass.

- [ ] **4. Save policy.**  The dirty-flag + debounce predicate,
  plus a unit test for the policy itself (no I/O — just the
  decisions).

- [ ] **5. TUI wiring.**  Startup dialog, plumb the policy into
  the TUI event loop, save inline on low-cadence mutations and
  on exit.

- [ ] **6. CLI surface.**  `--resume`, `--list`, exit message.

- [ ] **7. Integration tests** end-to-end through a synthetic
  engine.
