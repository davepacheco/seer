# Phase 1 — Orientation pass

A map of what is where, how it compares to the documented layout, and
which seams to look at more closely in later phases.

## Module layout, as it actually stands

Library modules (all flat under `src/`, except `engine` which contains
one submodule):

| Module             | Lines (approx) | Role                                          |
|--------------------|---------------:|-----------------------------------------------|
| `engine.rs`        | 1,355          | `Engine`, `EventStream`, `MergeIter`, `SourceCursor`, `ResolvePosition` |
| `engine/merge.rs`  | 1,326          | `Stepper`, `Cursor`, `MergeRecord`, `MergeError`, `SourceWindow`        |
| `streamview.rs`    | 2,557          | `StreamView` (viewport, search, anchoring)    |
| `source.rs`        | 1,598          | `Source` trait + `FileSource`; `SourceId`, `ByteOffset`, scan_* helpers |
| `filter.rs`        | 1,468          | `Filter`, `Predicate`, DSL parser/serializer   |
| `render.rs`        |   660          | `RenderOpts`, `HostnameDisplay`, `format_event`, `format_time` |
| `summary.rs`       | 1,089          | Field/time histograms                          |
| `session.rs`       |   402          | `Session`, `Tab`, `TabKind`, `Bookmark`        |
| `session_store.rs` |   814          | On-disk persistence + `SessionId`, `SessionMatch` |
| `seeit_target.rs`  |   908          | Resolve a `session+selector` into source/filter/cursor/render inputs |
| `stream.rs`        |   228          | `LogStream`, `LogStreamId`, `LogStreamPosition` |
| `save_policy.rs`   |   218          | Dirty bit + debounce timer                     |
| `event.rs`         |   363          | `Event`, `Level`, `LoggerName`, `Hostname`, `Pid` |

Binaries:

| File              | Lines    | Notes                                          |
|-------------------|---------:|------------------------------------------------|
| `bin/seer.rs`     | 10,269   | Single file; ~80 top-level items; `App` impl spans ~2000 lines (`src/bin/seer.rs:1983`); `Tab` impl ~450 lines (`src/bin/seer.rs:1468`). |
| `bin/seeit.rs`    |   804    | Reasonably compact.                            |

Tests:

- Unit tests live inline in each module.
- `tests/seeit_session.rs`, `tests/session_lifecycle.rs`,
  `tests/session_schema.rs` — integration tests.
- `tests/fixtures/` — checked-in golden files for the session schema
  tripwire test.
- `examples/gen_fixture.rs` regenerates them.

## Reality vs. the documented layout in `CLAUDE.md`

CLAUDE.md sketches an aspirational layout with `storage/`, `engine/`,
and most data types nested inside `engine/`.  The real layout is
flatter:

- No `storage/` submodule.  `source.rs` plays that role at the top
  level.
- `filter.rs`, `render.rs`, `stream.rs`, `session.rs`,
  `session_store.rs` are top-level peers rather than nested under
  `engine`.
- `engine/` exists but contains only `merge.rs`; `engine.rs` is its
  parent.

The flatter layout is not necessarily worse — the CLAUDE.md sketch is
explicitly an "example to give the flavor".  But the spread of types
across so many top-level modules means the eventual crate-extraction
split is not obvious from the tree.  Worth a closer look in Phase 2.

## Top-level types worth keeping in mind

Newtypes (raw type in parentheses):
`SourceId(String)`, `ByteOffset(u64)`, `Pid(u32)`,
`Hostname(String)`, `LoggerName(String)`, `BookmarkId(Uuid)`,
`BookmarkName(String)`, `LogStreamId(Uuid)`,
`SessionId([u8; 4])`.

Enums with non-trivial design weight:
`Level`, `Direction`, `HostnameDisplay`, `TabKind`, `Cadence`,
`MatchKind`, `Selector`, `ResolvedMode`, `ResolvePosition`,
`SearchOutcome`, `SearchDir`, `WindowFillStatus`, `SourceError`,
`MergeError`.

The single trait in the codebase is `Source` (`src/source.rs:194`),
with a single in-tree implementation (`FileSource`).  It is the
extraction seam for archive/network sources later.

## Seams to look at in later phases

These are observations to chase, not yet conclusions.

1. **Two parallel k-way merges**.  `engine.rs` builds its own
   `MergeIter` / `SourceCursor` pair (`src/engine.rs:592` / `:439`)
   for `EventStream` and `resolve_position`; `engine/merge.rs` builds
   `Stepper` / `SourceWindow` (`src/engine/merge.rs:395` / `:195`)
   for navigation.  Both implement out-of-order detection, the
   "error head wins" rule, and tie-breaking by source-add order.
   Worth confirming in Phase 4 whether the two paths can share more
   plumbing.

2. **Three stepper constructors that strictly nest**.  `stepper`,
   `stepper_with_batch`, `stepper_with_bounds` (`src/engine.rs:75,
   :92, :112`) duplicate the source-collection loop and delegate to
   progressively richer constructors.  Likely a candidate for a
   builder or for a single method with defaults.

3. **`resolve_position` has a sibling helper, `filtered_out_neighbor_by_time`**
   (`src/engine.rs:381`).  Same loop shape, anchored by time rather
   than by position.  Worth examining whether the two cases can be
   unified.

4. **`StreamView` is a 1,400-line `impl` block** (`src/streamview.rs:267`).
   The doc comment says it owns "plain data — no borrowed Engine
   reference", but it also embeds search-resume state, anchor logic,
   time navigation, and rendering.  Worth checking whether some of
   that ought to live elsewhere.

5. **`bin/seer.rs` is a single 10K-line file** with ~80 top-level
   items, including the entire `App` impl (`src/bin/seer.rs:1983`,
   ~2000 lines) and `Tab` impl (`:1468`, ~450 lines).  The TUI is
   inherently larger than the engine, but a single file at that
   size is hard to navigate.  Phase 2 should look at whether
   pieces — input handling, rendering, dialog logic — could be
   split out.

6. **`BATCH_SIZE = 64` is duplicated**.  `src/engine/merge.rs:48`
   and `src/streamview.rs:39` both define a 64-record batch.  The
   `streamview` comment notes the value must match the storage
   layer's, but the constant lives in both files.

7. **`LogStream` mirrors `RenderOpts` field-by-field**
   (`src/stream.rs:121`).  Six bool/enum fields are listed on the
   struct alongside `from`/`set_render_opts` methods that copy
   between the two.  The doc comment justifies it as
   schema-evolution friendliness ("individual fields so the session
   schema can evolve one knob at a time").  Worth checking whether
   `RenderOpts` could itself carry the `serde` and per-field
   defaults so the mirror isn't needed.

8. **`SourceMetadata::excludes_all` only handles `FieldEquals` for
   `name`/`hostname`** (`src/source.rs:330`).  Documented; the
   surrounding text says other predicates fall through to per-line
   evaluation.  Not a problem per se — flagging because the
   `match … else { continue; }` shape is a common silent-failure
   pattern when new `Predicate` variants land.

9. **`bin/seeit.rs` re-implements record emission** via
   `emit_forward_from_engine` / `emit_records_window` /
   `emit_record`.  The shape is intentionally distinct from
   `StreamView` (it's the non-windowed path).  Phase 2 should check
   whether the renderer abstraction makes that genuinely small or
   whether duplication has crept in.

10. **`Predicate::SourceIdMatches` short-circuits to `true`**
    in `Predicate::matches` (`src/filter.rs:192`) because the
    engine has already filtered the source out before per-event
    matching.  The comment is good; the structural shape (one
    variant whose runtime predicate is a lie) is the kind of thing
    Phase 3 should consider — would splitting `Predicate` into an
    `EventPredicate` and a `SourcePredicate` be cleaner?

## What I did not yet look at in detail

- The full body of `bin/seer.rs` (only skimmed table-of-contents).
- The body of `streamview.rs` past line 200.
- The bodies of `engine/merge.rs` past `Stepper::new`'s neighborhood.
- The integration test suite under `tests/`.
- `examples/gen_fixture.rs`.

Phase 2 will read into the bodies of the larger files to evaluate
layer adherence and locate concerns that escape their layer.
