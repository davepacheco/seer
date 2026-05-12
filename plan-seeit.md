# Plan: `seeit` session reproduction

## Goal

Make `seeit` able to reproduce anything visible in `seer`: pointed at a
session id plus a selector (bookmark / stream / tab / file), it emits the
same lines or summary the TUI would show.  Single-file mode (today's
`seeit <files> --filter ...`) stays as-is for development and integration
testing.

Final phase adds a `seer` keybinding that prints the `seeit` command line
equivalent to the current view, so a user can copy/paste to reproduce or
redirect output.

## Design (agreed)

### Targets

| target          | selector flags                            | starting position                     | output mode      |
|-----------------|-------------------------------------------|---------------------------------------|------------------|
| whole session   | `--session ID`                            | start of every source in the session  | records          |
| stream          | `--session ID --stream NAME`              | start of source set under that filter | records          |
| tab             | `--session ID --tab NAME`                 | tab's saved `cursor` (or start)       | records or summary, per `TabKind` |
| bookmark        | `--session ID --bookmark NAME-OR-PREFIX`  | the bookmark's `cursor`               | records          |
| specific file   | `<files...>` (today's positional)         | start of file                         | records          |

- `NAME` for stream/tab is `LogStream::name`.  Tab name == stream name in
  the current model.
- Bookmark `NAME-OR-PREFIX` accepts either a `BookmarkName` or a UUID
  prefix.  Ambiguity is an error that lists candidates.
- Selectors are mutually exclusive within a session.

### What gets reproduced

When a session target is used, `seeit` matches `seer` byte-for-byte:

- **Source set** = `Session::sources` (canonical paths).  Each source's
  fingerprint (mtime + size) must equal the value captured at session
  open; mismatch is a hard error for now.
- **Filter** = the target stream's `Filter`.
- **Render opts** = the stream's `show_extras` / `show_date` /
  `hostname_display` / `show_pid` / `show_name` / `show_raw`.
- **Mode** = records for stream/bookmark/file; for tab targets, mode
  comes from `TabKind` (Stream → records; Summary → summary histogram).

### Counts and direction

- Default: unbounded forward from the start position.
- `--count N`: stop after N emitted records.
- `--before N`: emit N records *before* the start position first.  Used
  for getting context around a bookmark.  Not the default — bookmark
  navigation in `seer` places the bookmark at the top of the viewport,
  not in the middle.

### Overrides (for testing)

These layer on top of whatever the session/target gives, in this order:

- `--filter EXPR`: replaces the resolved filter.
- `--and-filter EXPR`: ANDs onto the resolved filter.
- `--show-extras` / `--no-extras`, `--hostname={full,short,none}`,
  `--show-date` / `--no-date`, `--show-pid` / `--no-pid`,
  `--show-name` / `--no-name`, `--show-raw` / `--no-raw`: override
  render opts.

In file-only mode the current `--filter` keeps its meaning; the
override flags also apply (replacing today's hard-coded maximalist
`RenderOpts`).

### Code sharing with `seer`

This is a code-sharing exercise, not a re-implementation:

- A new library helper resolves `(SessionId, Selector)` into a
  `ResolvedTarget { sources, filter, render_opts, cursor, kind }`.
  `seer`'s `restore_tabs_or_default` already does most of this for tab
  restore; the helper is the extracted, testable form.
- Record emission goes through the existing `Engine::stepper` +
  `format_event`.
- Summary mode goes through the existing `SummaryBuilder` +
  `format_summary` (the same call shape `seer` uses for its Summary
  tabs).

## Open questions resolved

- Summary tabs: yes, render via `format_summary`.
- Specific file target: kept.
- Default count: unbounded.
- Tab identifier: stream name.
- Fingerprint mismatch: hard error (no override flag for now).
- Bookmark default direction: forward (matches `seer`).
- Sharing vs. reimplementing: sharing — extract resolver helper.

## Status

**Current phase: Phase 2 — target-resolution library helper.**  Phase 1
landed (CLI restructure committed).  Update the "Status" line and check
the phase box as work lands.

### Notes from Phase 1

- clap 4.6's per-arg `requires` and group `requires` are not enforced
  when the input group is satisfied by `files`; cross-arg invariants
  live in `Args::validate` instead.  Phase 5 should consider whether
  to convert this error type into a nicer Display in `main` (today it
  surfaces via `Debug`).

## Phases

### [x] Phase 1 — CLI restructure for `seeit` (no behavior change)

- Restructure `Args` in `src/bin/seeit.rs` to add `--session`,
  `--stream`, `--tab`, `--bookmark`, `--count`, `--before`, `--filter`,
  `--and-filter`, and the render-override flags.
- Enforce mutual exclusion: at most one of `--stream`/`--tab`/`--bookmark`;
  selectors require `--session`; `--session` and positional files are
  mutually exclusive.
- File-only mode (positional files + `--filter`) behaves identically to
  today.
- Tests: clap parse tests for every valid combination and the rejection
  cases.

### [ ] Phase 2 — Library: target resolution

- New module (likely `src/seeit_target.rs`, exposed from `lib.rs`)
  defining:
  - `Selector` enum (`WholeSession`, `Stream`, `Tab`, `Bookmark`).
  - `ResolvedTarget { sources, filter, render_opts, cursor, mode }`.
  - `resolve(store, session_id, selector) -> Result<ResolvedTarget, _>`.
- Source-fingerprint check with a typed error carrying expected vs.
  actual mtime/size; surfaces a clean message at the CLI.
- Name/prefix matching for streams and bookmarks; ambiguity error lists
  candidates.
- Refactor `seer`'s `restore_tabs_or_default` to call the same name/cursor
  pieces where it overlaps, rather than duplicate them.
- Unit tests for each selector and each failure mode.

### [ ] Phase 3 — Bounded emission with `--before`

- Audit whether `Stepper` already supports "N records ending at cursor
  C, then forward from C": its backward stepping likely covers this with
  a small wrapper that buffers N backward steps and emits them in order
  before resuming forward.
- If not, add the wrapper in the library (next to `Stepper`) rather than
  in the binary.
- Tests with hand-built fixtures: bookmarks at the start, middle, and
  end of a file; verify ordering and counts at edges.

### [ ] Phase 4 — Wire it up in `seeit.rs`

- When `--session` is supplied, call `resolve`, build the engine from
  the resolved sources, apply CLI overrides (filter then render opts)
  last, and emit per mode.
- Summary mode in `seeit`: drive `SummaryBuilder` over the engine and
  print `format_summary`.
- Integration tests under `tests/`: build a session via the library,
  save it, invoke the binary, snapshot stdout.

### [ ] Phase 5 — Diagnostics polish

- `--header` flag prints a one-line context banner to stderr (session
  id, target, starting cursor) for human-reading runs; never to stdout.
- Friendly errors for: unknown session id, unknown stream/bookmark
  name, ambiguous name, missing source, fingerprint mismatch (showing
  expected vs. actual).
- Update `seeit.rs` module doc and the CLI `about` string to describe
  session mode.

### [ ] Phase 6 — `seer` keybinding: "print equivalent `seeit`"

- New binding in `seer` that, given the active tab, prints to a chosen
  destination (stderr after exit / a popup with copy hint / `--echo-cmd`
  on exit — TBD) a shell-quotable `seeit` invocation reproducing the
  current view.
- The builder is the inverse of `resolve`: it reads the active tab,
  selects the target by stream name (or by bookmark name if the cursor
  matches one), and emits matching `--*` override flags only where the
  stream's render opts differ from the file-only defaults (to keep the
  command line short).
- Round-trip test: snapshot the rendered output of a `seer` view, run
  the printed `seeit` command, assert byte-equal output.
- A note in `CLAUDE.md` pointing at this as the canonical way to file a
  bug report against a specific view.

## Out of scope (for now)

- Override flag for fingerprint mismatch.
- Reproducing TUI-only chrome (status line, tab bar, search highlights).
- Reproducing in-progress / unsaved TUI edits — only the persisted
  session is the source of truth.
