# Seer — ownership overview

A map of the codebase intended for a developer who hasn't been hands-on
with the code and wants to take responsibility for it.  Read top-to-
bottom once; come back to sections as you dig in.

The companion `review/phase-1-orientation.md` lists modules and line
counts in tabular form.  This document is different: it tries to explain
*why* the pieces are shaped the way they are, what the load-bearing
abstractions are, how a keystroke travels through them, and where the
soft spots are.

---

## 1. The shape from 30,000 feet

One Cargo crate.  Library plus two binaries.

```
seer (library)
├── bin/seer    — interactive ratatui TUI
└── bin/seeit   — non-interactive CLI that emits formatted lines to stdout
```

Both binaries depend on the same library; the library does not know
about either binary.  The library is laid out so that the eventual
crate split (`storage`, `engine`, `tui-engine`) is one `cargo new -p`
away rather than a rewrite — see `CLAUDE.md` ("Module layout").

There are three conceptual layers, top to bottom:

| Layer        | Files                                                              | Knows about              | Does not know about               |
|--------------|--------------------------------------------------------------------|--------------------------|-----------------------------------|
| TUI binary   | `bin/seer.rs`                                                      | engine, ratatui          | (none — sits at the top)          |
| Engine       | `engine.rs`, `engine/merge.rs`, `stream.rs`, `streamview.rs`, `filter.rs`, `render.rs`, `summary.rs`, `view_target.rs`, `session.rs`, `session_store.rs`, `save_policy.rs`, `event.rs`, `position.rs` | storage              | ratatui, terminals                |
| Storage      | `source.rs` (+ `position.rs` for primitive types)                  | filesystem, the filter   | sessions, renderers, ratatui      |

`position.rs` holds the smallest, most persistent shapes (`SourceId`,
`ByteOffset`, `ByteLen`, `Cursor`, `LogStreamPosition`) at the bottom
of the dependency graph so any layer can reference them without
inducing cycles.  See the file header comment for the rationale.

---

## 2. The core types and their roles

There are a lot of types; here are the ones you'll see over and over.
Each one is a load-bearing abstraction — when you read code that uses
it, you mostly want to know what *invariant* it carries rather than
how it's implemented.

### Storage primitives

- **`SourceId`** (`position.rs:55`) — opaque string identifying a
  source.  For `FileSource` it's the canonicalized absolute path.
- **`ByteOffset`** (`position.rs:86`) — newtype around `u64`,
  positions a byte within a single source.  Convention: an offset
  always names the byte *at which the next record would start*.
- **`ByteLen`** (`position.rs:127`) — newtype companion.  The
  arithmetic the engine repeatedly does on byte counts is type-safe:
  `offset + len -> offset`; `len + len -> len`; mixing them any other
  way is a compile error.
- **`Cursor`** (`position.rs:224`) — a `BTreeMap<SourceId,
  ByteOffset>`, one byte offset per source.  This is the
  serializable bookmark for "where is the merge stepper right now."
  A default cursor walks every source from byte 0.

### Events

- **`Event`** (`event.rs:31`) — parsed bunyan record (time, level,
  name, hostname, pid, msg, v, plus arbitrary `extra` JSON fields).
  Today the only format; the deserializer tolerates duplicate keys
  by first-wins.
- **`Level`** (`event.rs:136`) — bunyan severity.  Variants are
  ordered by severity so derived `Ord` matches the obvious
  comparison.  The bunyan numeric mapping (10/20/30/40/50/60) is
  pinned by a single table at `event.rs:154`.

### The filter language

- **`Filter`** (`filter.rs:50`) — a conjunction of `Predicate`s.
  Empty filter accepts every event.  Parses from a small DSL
  (`name=Nexus level>=warn msg=~oops`).  `serde`-serializable.
- **`Predicate`** (`filter.rs:240`) — split into:
  - `EventPredicate` (`filter.rs:265`) — applied per event.
  - `SourcePredicate` (`filter.rs:300`) — applied per source-id.
  The split exists so that whole sources can be pruned at query
  time, before any of their bytes are read, via
  `Filter::matches_source_id`.
- **`Form`** (`filter.rs:105`) — `Affirmed` or `Negated`, so the
  parser doesn't carry naked `bool`s through the matching code.

### Sources

- **`Source`** trait (`source.rs:125`) — anything that produces
  events.  Two query primitives: `events()` (iterator over the whole
  source) and `query_bounded()` (random-access reads with direction
  and a walks budget).  Every implementation also exposes
  `SourceMetadata` so the engine can prune whole sources before
  opening them.
- **`FileSource`** (`source.rs:307`) — the only implementation
  today.  Canonicalizes its path on open; that becomes its
  `SourceId`.  Reads the first and last records on open to populate
  metadata for whole-file pruning (`source.rs:656`).
- **`Direction`** (`source.rs:36`), **`QueryRecord`** (`source.rs:63`),
  **`QueryBatch`** (`source.rs:88`) — the data shapes that flow
  through `query_bounded`.  The walks budget lets a long-running
  scan resume in small chunks rather than freezing the UI.

### The engine

- **`Engine`** (`engine.rs:43`) — owns the set of sources currently
  in play.  All higher layers go through it for log content.
- **`EventStream`** (`engine.rs:503`) — full-pass iterator over the
  merged stream, used by `summarize` and by `seeit`.
- **`Stepper`** (`engine/merge.rs:373`) — random-access merge over
  the sources, with per-source forward/backward buffers
  (`SourceWindow`) so scrolling doesn't re-read the file.  Powers
  every interactive navigation in the TUI.
- **`StepperOptions`** (`engine/merge.rs:346`) — knobs for batch
  size and per-fill walks budget.  The long-op driver passes a
  small budget so a sparse-filter scan tick yields to the UI.
- **`MergeRecord`** (`engine/merge.rs:66`) — what each step
  produces.  Contains the parsed event (or per-line error), the
  source id, the byte offset and length, and the raw bytes.
- **`LogStreamPosition`** (`position.rs:296`) — `(source, time,
  ordinal_within_time)`.  This is the *content-addressed* anchor
  bookmarks use; it survives filter changes because it doesn't name
  a row in a filtered view.

### Streams (the unit a tab views)

- **`LogStream`** (`stream.rs:59`) — id + display name + filter +
  per-stream render options (show extras, show date, hostname
  display, etc.).  Two tabs targeting the same stream share its
  filter; both refresh when it changes.
- **`LogStreamId`** (`stream.rs:38`) — a UUID.

### Rendering

- **`RenderOpts`** (`render.rs`) — five toggles that control how an
  event is formatted: show extras, show date prefix, hostname mode
  (short/full/none), show pid, show name, plus a `show_raw` escape
  hatch.  `LogStream` persists these per stream.
- **`format_event`** (`render.rs:185`) — the formatter used by both
  the TUI (via `StreamView::WindowEntry`) and the CLI.

### The TUI viewport

- **`StreamView`** (`streamview.rs:431`) — the load-bearing
  abstraction between engine and TUI.  Caches a bounded window
  (`WINDOW_SOFT_CAP = 1024` records, `streamview.rs:41`) around the
  viewport's top.  Anchored by `(record_key, line_within_record)`
  rather than a flat index so the anchor survives window trims.
  Holds `front_cursor` and `back_cursor` (engine cursors at the
  window edges) so extension/trim never re-reads the bytes already
  in the deque.
- **`Materialized`** (`streamview.rs:243`) — flat snapshot of the
  current window: per-record `events`, per-line `formatted`
  strings, and two index maps so callers can translate between
  "scroll position" and "the record under the cursor" without
  re-scanning.
- **`RecordKey`** (`streamview.rs:63`) — `(source_id, offset)`,
  stable across window slides.  Used as the cursor's identity for
  selection, search resumption, and anchor pinning.
- **`Anchor`** (private, `streamview.rs:305`) — viewport top.  Four
  variants: `PinFront`/`PinBack` are pre-resolution markers; `On`
  is the resolved state; `Empty` is for filter-rejects-all.
- **`SearchOutcome`**, **`SearchDir`**, **`SearchAnchor`**,
  **`WindowFillStatus`** — the protocol types used by the long-op
  driver in the TUI to interleave search/fill ticks with rendering.

### Summary

- **`Summary`**, **`SummaryBuilder`**, **`FieldSummary`**,
  **`TimeSummary`** (`summary.rs`) — field histograms and a time
  histogram over a filtered event stream.  Powers the Summary tab.
  `summarize()` builds one in a single pass; `format_summary()`
  produces the same flat `Vec<String>` shape the regular log view
  uses so the same viewport machinery can scroll it.

### Sessions and persistence

- **`Session`** (`session.rs:326`) — root persistent object.
  Sources, tabs (open at last save), streams (with their filters
  and render options), bookmarks per stream, search history.
- **`SessionId`** (`session.rs:49`) — 4-byte ID encoded as 8-char
  hex; user-typeable on the `seeit --session ID` command line.
- **`Bookmark`** (`session.rs:202`) — `BookmarkId` + cursor +
  optional name + cached display fields (time, source, name, msg)
  so the bookmarks tab can render even before the source has been
  opened.
- **`SessionStore`** (`session_store.rs:117`) — `$XDG_STATE_HOME/
  seer/sessions/` (or `$SEER_STATE_DIR/sessions/`).  Atomic writes
  via `*.tmp + rename`.  Schema is versioned (`CURRENT_SESSION_
  VERSION = 7`); `#[serde(default)]` on new fields lets older files
  round-trip cleanly.
- **`SavePolicy`** (`save_policy.rs:54`) — pure state machine,
  no I/O.  Two cadences: `Inline` (bookmarks, tab open/close,
  filter change — flush now) and `Debounced` (scrolling — coalesce
  over a window, default 10s).

### Resolution (the `seeit --session` bridge)

- **`Selector`** (`view_target.rs:36`) — what the user picks via
  CLI: `WholeSession`, `Stream(name)`, `Tab(name)`,
  `Bookmark(name_or_id_prefix)`.
- **`ResolvedTarget`** (`view_target.rs:70`) — what `seeit` needs
  to actually emit: source paths, filter, render options, starting
  cursor, mode (Records or Summary).
- **`resolve` / `resolve_in_session`** (`view_target.rs:233/247`) —
  one looks the session up on disk, the other takes an already-
  loaded session in memory; the TUI uses the in-memory path when
  generating the `Y`-key seeit command.
- **`build_seeit_command`** (`view_target.rs:477`) — shell-quotes
  the `seeit --session ... --tab ...` command for the current view.

---

## 3. How a keypress flows end-to-end

Three traces.  Each one threads a different stack so you see the
layering in action.

### Trace A: "open a file and render the first screen"

1. `seer ./Nexus.log` — `bin/seer.rs` parses args, opens a
   `SessionStore`, builds (or resumes) a `Session`, and constructs
   an `Engine` adding each path as a `FileSource`.
2. For each persisted tab (or one default tab), build a `Tab` with
   a fresh `StreamView` (`bin/seer.rs:Tab::new`) wrapping the
   stream's filter and render options.
3. The TUI's main loop (`bin/seer.rs:run_tui` around line 619) runs
   `terminal.draw(|frame| render(frame, &mut app))`.  Inside
   `render` (around line 5366) the active tab is identified, and
   `StreamView::ensure_window` is called if the window hasn't been
   filled yet.
4. `ensure_window` constructs a `Stepper` via `Engine::stepper`,
   walks `step_forward` until the deque is full or EOF.  Each
   record is wrapped in a `WindowEntry` (which calls `format_event`
   immediately, so per-frame rendering is just slicing a
   pre-formatted `Vec<String>`).
5. The viewport's visible slice of `materialized.formatted` is
   handed to a ratatui `Paragraph`.  Status, tab bar, and any open
   dialog are drawn over the same frame.

### Trace B: "press `j` at the bottom of the window"

1. `event::poll(100ms)` in the main loop returns a key event.
2. `App::handle_key` (around `bin/seer.rs:3851`) — large match on
   `key.code` + modifiers — dispatches `j` to a scroll-down handler
   on the active tab.
3. The handler calls `Tab::scroll_lines(1)` → `StreamView::
   scroll_lines(1)`.  Inside, the anchor advances one line within
   the current record, or to the next record's line 0, and if the
   viewport is now within `OVER_FETCH_LINES` (128) of the back of
   the deque, `extend_forward_batch` fires.
4. `extend_forward_batch` reuses `back_cursor`, builds a fresh
   `Stepper`, walks `FETCH_BATCH_SIZE` (64) records forward, pushes
   them onto the deque, advances `back_cursor`, then trims the
   front if the deque exceeded `WINDOW_SOFT_CAP`.
5. `recompute_materialized` runs at the tail of the mutator; the
   next frame's `render` pulls a fresh `materialized` slice.

### Trace C: "press `f`, type a filter, hit Enter"

1. `f` opens `Dialog::Filter` (`bin/seer.rs:Dialog` at line 4507).
   The dialog owns a `LineEditor`.  All keys now route through the
   dialog's `handle_key`.
2. Typing edits the `LineEditor` buffer.  Each frame re-renders the
   dialog with the current buffer and any parse error from a
   live-attempted `Filter::from_str`.
3. Enter → `DialogResult::ApplyFilter(filter)` → `App::apply_
   filter` (around `bin/seer.rs:2781`).  This:
   - mutates `LogStream::filter` on the active stream;
   - captures the current anchor as a `Cursor` via `StreamView::
     cursor_at_anchor`;
   - on each affected tab, calls `StreamView::set_filter` (which
     drops the cache) and then enqueues a `LongOp::Seek` that
     drives `ensure_window_step` chunk-by-chunk back toward the
     captured cursor;
   - calls `SavePolicy::record(Cadence::Inline)` so the session
     flushes on the next loop tick.
4. The main loop's `advance_long_op` runs one chunk per frame,
   showing a progress bar in the status line until
   `WindowFillStatus::Done`.  Then the user's anchor lands on the
   same record (if it still passes the new filter) or the nearest
   one that does.

The thing to take away from all three traces: the TUI is *not*
concurrent.  Everything runs on the main thread, including engine
I/O.  The long-op machinery turns potentially-blocking operations
into a sequence of bounded ticks that the main loop interleaves
with key polling and rendering.  See "long-op machinery" below.

---

## 4. Subsystems that cut across the layering

### Dialog system

`Dialog` enum (`bin/seer.rs:4507`).  Each variant carries its own
state (most have a `LineEditor`).  When `app.dialog.is_some()`,
`handle_key` short-circuits and routes through `Dialog::handle_key`
(line 4851), which returns a `DialogResult` telling the app what
just happened (Cancel, Apply variant per dialog).  Rendering is
similarly two-stage: `render_dialog` (line 5937) dispatches per
variant.

Dialogs in flight: `Filter`, `Rename`, `Search`, `BookmarkName`,
`ConfirmDeleteBookmark`, `ConfirmQuit`, `DisplayFields`,
`SeeitCommand`, `Help`.  Search is special — its prompt is rendered
inline at the bottom rather than as a centered modal.

### Key bindings

Not centralized.  The default-mode bindings live in `App::handle_
key` as a long `match` on `key.code` (line ~3923 onward).  Dialog
bindings live in `Dialog::handle_key`.  The bookmarks pane has its
own short keymap.  Select mode (set after `x`/`X`/`b`) suppresses
non-navigation keys.

This is a known soft spot.  A central binding table would make
help generation, customization, and conflict detection easier.  The
existing `Help` dialog is hand-written to mirror what the match
arms do.

### Long-op machinery

The driver lives in `App::advance_long_op` (around `bin/seer.rs:
2317`).  Three operation types, each one a chunk-at-a-time state
machine:

- **`SeekOp`** — walks `StreamView::ensure_window_step` repeatedly
  to fill the window after a `g`/`G`/filter change/bookmark jump
  with a selective filter applied.
- **`SearchOp`** — calls `StreamView::search_step_with_budget`,
  honoring the `SEARCH_BUDGET = 50_000` records-per-tick cap.
- **`SummaryOp`** — calls `SummaryBuilder::fold_records` with a
  per-tick record budget, then `finalize` once the pass is done.

The main loop polls `event::poll(Duration::ZERO)` instead of 100ms
while a long-op is active, so Ctrl-C cancels promptly.  Each `Long
Op` exposes `bytes_done` / `total_bytes` / `records` accessors used
by `format_long_op_progress` (line ~1237) to render a status-line
bar.

This is the most subtle subsystem in the codebase.  It exists
because real Oxide log bundles can take 1-2 minutes per file to
parse, and a naive "open ten files, scan everything" startup is
unusable.  Read the `StreamView::ensure_window_step` rustdoc
carefully when you get to it.

### Session save policy

Two cadences (`save_policy.rs`).  Bookmark creation flushes
immediately (cheap, user-visible, never lose).  Scrolling marks the
session dirty and re-arms a debounce timer (default 10 s); the next
loop tick that finds `policy.due()` true flushes.  On clean exit,
`policy.dirty()` is checked one last time.

`SessionStore::save` writes atomically (`*.tmp` then rename).  The
schema is versioned; `CURRENT_SESSION_VERSION = 7` today.  A
`tests/session_schema.rs` fixture test fails if the JSON shape
drifts without a version bump.

### Window/cursor algebra

The trickiest invariants in the codebase live in `StreamView`'s
deque-plus-two-cursors model.

- `front_cursor` is where a fresh `Stepper`'s `step_forward` would
  return `records.front()`.
- `back_cursor` is where a fresh `Stepper`'s `step_forward` would
  return the record *after* `records.back()`.
- Extending forward advances `back_cursor` by the bytes of the
  newly-appended records.  Trimming the front advances `front_
  cursor` by the bytes of the dropped records.  Symmetric for
  backward.
- `cursor_before_record(idx)` is derived from `front_cursor` plus
  the records before `idx` in the deque, without I/O.  This is
  what bookmark resolution and "preserve position across filter
  change" rely on.

When the math gets confusing, the source-of-truth invariants are
in the rustdoc on `streamview.rs:431`-ish (the `StreamView` struct)
and `streamview.rs:155`-ish (the `SourceWindow` struct inside
`engine/merge.rs`).

---

## 5. Soft spots and places I'm least sure about

A candid list of things you might want to look at hard before
signing off.

1. **`bin/seer.rs` is 11,866 lines in one file.**  The other
   modules are well-bounded.  The TUI binary has grown a lot of
   responsibility — `App`, every dialog variant, the long-op
   driver, the rendering, the key dispatch.  Splitting this into
   submodules (`bin/seer/app.rs`, `bin/seer/dialog.rs`, ...) is on
   the follow-up list and would help future you.

2. **Key bindings are scattered through match arms.**  No central
   table.  The `Help` dialog content is hand-written.  Adding a
   binding is easy; auditing the full set requires reading three
   different `handle_key` impls.

3. **Long-op interleaving is correct but subtle.**  Specifically,
   `ensure_window_step` has a multi-phase state machine (populate,
   resolve pinned anchor, look-ahead).  `CLAUDE.md` already calls
   out a few known long-op coverage gaps (`scroll_lines` at window
   edge, `advance_time`, `SeekFinalize::FrontOrBackFallback`).  See
   that file's "Misc TODO" section.

4. **Whole-file pruning relies on metadata.name being uniform
   within a file.**  This is true of every Oxide bunyan file in
   practice but the heuristic is documented as such in
   `source.rs:266`.  A mixed file would silently produce missing
   records.  If you ever see "I know my filter matches but seer
   says nothing's there," start here.

5. **Out-of-order detection is one-shot per source.**  The first
   timestamp regression in a source emits an `OutOfOrder` warning
   inline; subsequent regressions are silent (`engine.rs:401`).
   The trade-off is right — a badly unsorted file shouldn't drown
   its real entries — but it does mean that if you depend on
   "every regression was reported," you'll be surprised.

6. **`StreamView::recompute_materialized` rebuilds the flat
   projection on every mutator.**  O(window_size), microseconds in
   practice, but it's still a full rebuild rather than an in-place
   patch.  Worth knowing if you ever profile a UI hot path.

7. **`Cursor` equality is not normalized.**  Two cursors that
   navigate identically can compare unequal as `BTreeMap`s when one
   omits a source that the other maps to `ByteOffset::ZERO`.
   Documented at `position.rs:204`, but a future caller that
   compares cursors for logical equality will need to walk the
   shared key set rather than rely on `==`.

8. **`bin/seeit.rs` is reasonably compact (814 lines) but does
   double duty for file mode and session mode.**  The argument
   parser has explicit cross-arg invariants (`ArgValidateError`)
   because clap's `requires` machinery can't fully express the
   constraints.  If you're adding a new flag, look there first.

9. **Session schema migrations don't exist yet.**  New fields use
   `#[serde(default)]`.  When you change shapes more substantially
   (rename a field, restructure an enum), you'll need to write a
   migration shim keyed on `session.version`.  The schema-tripwire
   test will tell you when that day comes.

10. **No async, no thread pool.**  Single-threaded TUI with chunked
    operations on the main thread.  This is a deliberate choice (per
    the project goals — simple ownership, no actor framework yet) but
    it means a future "fetch ten files in parallel" wants its own
    design conversation.

---

## 6. Where the tests live

- Unit tests are inline in each module (`mod tests` blocks).
- Integration tests are under `tests/`:
  - `seeit_session.rs` — exercises `seeit --session ...` against
    pre-built sessions
  - `session_lifecycle.rs` — open/save/load round-trips
  - `session_schema.rs` — version tripwire
  - `scale.rs` — feature-gated; uses `test-fixtures` to build large
    inputs
- Benches: `benches/engine.rs`.
- `src/test_fixtures.rs` is the shared bunyan-record-building
  helper (`append_bunyan_at`, `t(secs)`, `TestDir`).

The CLI exists in part to make integration testing cheap: stdin/
stdout fixtures verify the engine end-to-end without a terminal.

---

## 7. Suggested reading order if you want to go deeper

Top-to-bottom of the layer stack, smallest to largest:

1. `position.rs` (330 lines) — the primitives.
2. `event.rs` (363 lines) — what a parsed record looks like.
3. `filter.rs` (1,865 lines) — the filter DSL, predicates, matching.
   Skim past the parser fast path the first time.
4. `source.rs` (1,539 lines) — the `Source` trait and `FileSource`.
   Read the rustdoc on `query_bounded` carefully.
5. `engine.rs` + `engine/merge.rs` (1,342 + 1,273 lines) — the
   merge stepper.  This is where the time-merge invariants live.
6. `streamview.rs` (2,876 lines) — windowing and the long-op
   protocol.  Read the struct doc on `StreamView` first, then
   `ensure_window_step`, then come back to the rest.
7. `render.rs`, `summary.rs` — pretty self-contained.
8. `session.rs`, `session_store.rs`, `save_policy.rs`,
   `view_target.rs` — persistence layer.  Read `view_target.rs`
   last; it's the bridge that ties selectors to engine inputs.
9. `bin/seer.rs` (11,866 lines) — the TUI.  Read `App`, then the
   main loop in `run_tui`, then `render`, then `Dialog`, then the
   long-op driver, then the keymap.
10. `bin/seeit.rs` (814 lines) — the CLI binary, easier to read in
    one sitting once the engine and `view_target` make sense.

When something doesn't click, search `review/phase-1-orientation.
md` through `review/phase-5-tests.md` — there's a lot of
already-captured commentary on the same questions you'll hit.
