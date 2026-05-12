# Phase 2 — Layer-line review

CLAUDE.md states three rules for the library:

> - Binaries depend on the library; the library does not know about either binary.
> - `engine` depends on `storage`. `storage` does not know about
>   purely user-level concerns (like projects or log entry rendering),
>   but it could know about things like filters if it makes sense to
>   push the logic down to this layer for performance.
> - `engine::view_state` and `engine::session` are TUI-only members of
>   the engine module — the CLI binary doesn't import them.
> - Neither `engine` nor `storage` knows about ratatui.

The library follows the no-ratatui rule cleanly: `grep -nR ratatui src/`
shows zero hits outside `bin/seer.rs`.  The other rules are mostly
honored but a few seams have drifted.

## Dependency edges (library-internal `use crate::*`)

```
event           ← (leaf)
save_policy     ← (leaf)
render          ← event
source          ← event, filter             ⚠ ① cycle with filter
filter          ← event, source             ⚠ ① cycle with source
stream          ← filter, render, source
engine          ← event, filter, source, stream
engine/merge    ← event, filter, source
streamview      ← engine, event, filter, render, source
summary         ← engine, event, filter, source
session         ← engine, session_store, source, stream  ⚠ ② cycle
session_store   ← session                                ⚠ ② cycle
seeit_target    ← engine, filter, render, session,
                  session_store, stream
```

The two ⚠ marks are real module-level cycles that compile under
Rust's relaxed within-crate rules but would block any future crate
extraction along those seams.

### Finding 1 — `filter` ↔ `source` cyclic dependency

- `src/filter.rs:37` imports `SourceId` (for `Predicate::SourceIdMatches`
  and `Filter::matches_source_id`).
- `src/source.rs:20` imports `Filter` and `Predicate` (for
  `SourceMetadata::excludes_all` and the `Source::query`/`query_bounded`
  signature).

The cycle is small — both directions go through one type — but it
means neither `filter` nor `source` can be extracted to its own
crate without one of:

- moving `SourceId` to a common types module, or
- having `Source::query` accept a generic predicate trait rather than
  a `&Filter`, or
- collapsing both into one module.

CLAUDE.md explicitly permits storage to know about filters "if it
makes sense to push the logic down to this layer for performance" —
which is the right call for `SourceMetadata::excludes_all`'s
whole-file pruning.  The cycle is therefore intentional in spirit;
the question is whether `SourceId` (a small, content-free type)
ought to live outside `source.rs` so the cycle is broken at the
type level even when the runtime call structure is the same.

### Finding 2 — `session` ↔ `session_store` cyclic dependency

- `src/session.rs:23` imports `SessionId` from `session_store`.
- `src/session_store.rs:23` imports `Session` from `session`.

`SessionId` is logically a property of a `Session`; it would sit
more naturally in `session.rs`.  `session_store` would then depend
on `session` one-way.  This looks like a placement accident rather
than a designed layering.

## Layer-line drift

### Finding 3 — `engine::Cursor` re-exported and used by `session`

`engine/merge.rs` defines `Cursor` (a `BTreeMap<SourceId, ByteOffset>`
wrapper) for the navigation stepper.  It is then re-exported at the
library root and used by:

- `session::Tab::cursor` (`src/session.rs:156`),
- `session::Bookmark::cursor` (`src/session.rs:106`),
- `seeit_target::ResolvedTarget::cursor`,
- the App in `bin/seer.rs`.

`Cursor` is a perfectly reasonable persistence-friendly type, but
its current home is "internal helper to the merge stepper" — see
the doc comment at `src/engine/merge.rs:57`.  Whenever an internal
helper type becomes a persisted type that other modules and the
session schema depend on, it deserves a more prominent home —
either lifted to `source.rs` next to `ByteOffset`, or to a top-level
`position.rs` alongside `LogStreamPosition`.  Today it sits at the
back of a 1300-line file even though half the codebase depends on
its shape.

### Finding 4 — `LogStreamPosition` lives in `stream`, only the engine produces it

`src/stream.rs:75` defines `LogStreamPosition`.  The type is only
*constructed* by `engine.rs`'s `SourceCursor::fill`
(`src/engine.rs:506`) and by the bookkeeping in
`bin/seer.rs:materialize_streamview` (`:806`).  It's *consumed* by
`Engine::resolve_position`, the session's `Bookmark::display_*`
fields, and the TUI's selection logic.

`stream.rs` is otherwise a thin file (228 lines) holding
`LogStream`, `LogStreamId`, and the position type.  `LogStream`
itself is session data — it sits in `Session::streams` and is
serialized.  `LogStreamPosition` is engine output that happens to
get persisted in bookmarks.  Two unrelated concepts in one module.
Worth splitting: position alongside the cursor (Finding 3),
LogStream into session.

### Finding 5 — `Tab` keeps a `StreamView` *and* parallel flat arrays

`bin/seer.rs:1396` declares `Tab` with:

```rust
streamview: Option<StreamView>,
events: Vec<Option<EngineEvent>>,
formatted: Vec<String>,
event_for_line: Vec<usize>,
first_line_for_event: Vec<usize>,
```

The flat arrays are produced by `materialize_streamview`
(`src/bin/seer.rs:789`), which walks the `StreamView`'s deque on
every refresh / rerender / scroll-past-edge.  The streamview is the
source of truth; the flat arrays are a TUI-shaped projection of it.

Two consequences:

1. Every navigation event has to keep the two in sync.  The
   comments on `refresh` and `rerender` are about ten lines each on
   what gets cleared / preserved.
2. `materialize_streamview` recomputes a window-relative
   `LogStreamPosition` per event because `MergeRecord` (from
   `engine/merge.rs`) doesn't carry one — even though the
   non-stepper merge path (`SourceCursor::fill`) already knows how
   to mint one.  See Finding 8 in Phase 1.

Two possible directions, both for later:

- Move the materialization onto `StreamView` itself (return a
  `&Materialized` borrowed view) so the Tab need not hold a parallel
  copy; or
- Decide that the flat representation *is* what the TUI wants and
  reduce `StreamView` to a fetch-window engine with no public
  iterator at all.

The current shape is the worst of both: two materializations of
the same window, with synchronization rules spelled out in prose.

### Finding 6 — Two `ParseStats` types

- `streamview::ParseStats` (`src/streamview.rs:153`): records,
  bytes, walked_bytes, elapsed.
- A second `ParseStats` (`src/bin/seer.rs:683`): records, bytes,
  elapsed — no `walked_bytes`.

`Tab.parse_stats` is the binary's type; it is populated by
copying out fields from `streamview::ParseStats` and dropping
`walked_bytes`.  The progress bar reads `walked_bytes` from the
streamview directly when a long-op is active, then falls back to
`Tab.parse_stats` between ops.

This looks like a pre-`walked_bytes` design that survived its
generalization.  The streamview type subsumes the binary type;
the binary's `ParseStats` could be deleted and the streamview's
type used everywhere.

### Finding 7 — `LongOp` machinery lives in the binary

`bin/seer.rs:939` to `:1240` holds `LongOp` plus `SummaryOp`,
`SearchOp`, `SeekOp`.  These are essentially a chunked driver
around library-level operations (`SummaryBuilder`, `Stepper`,
`StreamView::search_step_with_budget`).  The driver carries no
ratatui or terminal state; it tracks bytes/records/eof and exposes
`bytes_done` / `total_bytes` / `records` / `label`.

The TUI consumes it via a progress bar, but the chunking logic is
fundamentally about the engine, not the terminal.  Moving it into
the library (e.g. `engine::long_op` or a top-level `long_op.rs`)
would:

- make it unit-testable without spinning up an `App`,
- let `seeit` reuse the same chunked machinery if a future
  `seeit --progress` mode lands,
- shrink `bin/seer.rs` by 300 lines.

Not urgent; flagged as future cleanup.

### Finding 8 — `engine` has two parallel k-way merges

(Flagged in Phase 1.)  `engine.rs` builds its own
`SourceCursor` + `MergeIter` (lines 439, 592) for the eager
`EventStream` path used by `query_events`, `resolve_position`, and
`cursor_for_position`.  `engine/merge.rs` builds
`SourceWindow` + `Stepper` for the lazy navigation path.  Both
implement the "error head wins over event head; ties by source-add
order" rule and out-of-order detection.

This is a layer-line issue because the duplication means engine has
two notions of "what comes next" that must stay in lockstep.  The
non-stepper path emits an `EngineEvent` carrying a
`LogStreamPosition`; the stepper emits a `MergeRecord` without one,
which is what forces `materialize_streamview` to recompute
ordinals.  A unified output type would eliminate that recomputation.

Phase 4 will look at unification more concretely.

### Finding 9 — `seeit_target` is the right idea in the wrong file name

`seeit_target.rs` resolves a `(session_id, selector)` into the
inputs needed to reproduce a saved view.  It's pure data
manipulation — no I/O beyond the session store, no rendering.  The
shape is general (it's the same machinery `seer` uses to print a
`seeit` command for the active view); only the name is binary-
specific.

Renaming to something like `view_target.rs` or `resolved_view.rs`
would clarify that it serves both directions.  Cheap change; not
load-bearing.

### Finding 10 — `streamview` is correctly above `engine`

The doc comment on `StreamView` says it "owns plain data — no
borrowed Engine reference" and "constructs a fresh Stepper on every
fetch".  In practice the methods do take `&Engine` parameters.  The
spirit is right: the streamview's state is plain data; the engine
is borrowed only for the duration of a fetch.  This is the cleanest
layer line in the codebase.

## Things that look clean

- No `ratatui` imports outside `bin/seer.rs`.
- `seeit.rs` does not import streamview, render dialogs, or any TUI
  shape.  It uses `Engine`, `Filter`, `Stepper`, `summarize`, and
  `format_summary` directly — exactly the engine's public surface.
- `event`, `save_policy`, and `test_util` are leaves and stay so.
- `summary` depends only on `engine` + plain types; the single-pass
  builder is well isolated.
- `render` is correctly minimal: it knows about `Event` and nothing
  else.

## Concrete cleanup candidates, ranked by leverage

1. **Move `SessionId` from `session_store` into `session`.**  Breaks
   one cycle.  Pure rename.
2. **Move `Cursor` (engine/merge → source or a new top-level
   module) and `LogStreamPosition` (stream → same).**  Reflects
   their role as the codebase's persisted-position primitives.
3. **Drop the binary's `ParseStats`** in favor of the streamview's.
4. **Decide whether the binary's `Tab` should keep parallel flat
   arrays** or whether `StreamView` should grow a flat-view method.
   Either resolves Finding 5; the current "both" is the only wrong
   answer.
5. **Lift `LongOp` and its three op-structs into the library.**
   Big readability win for `bin/seer.rs`.
6. **Decide on filter ↔ source separation.**  Either accept the
   cycle (it's small) and document it, or lift `SourceId` into a
   common place.

None of these are correctness issues today.  They show up as
synchronization rules in comments, extra fields, and parallel
implementations.
