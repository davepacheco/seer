# Phase 4 — Abstraction and complexity

Findings:

- **A** — unnecessary or redundant abstractions
- **B** — abstractions in the wrong place (re-stated from Phase 2 if
  they're the same concern)
- **C** — places where the *lack* of an abstraction means duplicated
  or fragile code

## A. Unnecessary / redundant

### A1 — Three forwarding stepper constructors

`src/engine/merge.rs:415, :426, :438` define `Stepper::new`,
`with_batch_size`, and `with_bounds`, where the first two are pure
delegations to the third with default arguments.  `engine.rs:75,
:92, :112` mirror this with three engine-level wrappers, each
duplicating the source-collection loop.  Six methods that boil
down to one builder.

**Fix:** Either expose a single `Engine::stepper(filter, cursor)` that
returns a `StepperBuilder` (with `.batch_size(...)`, `.max_walks(...)`),
or simplify down to one `stepper_with_bounds` plus default constants —
the long-op driver and tests only need two or three call shapes in
practice.

### A2 — Two parallel k-way merges in `engine`

(Restated from Phase 1 finding 1 and Phase 2 finding 8.)

- `EventStream` + `SourceCursor` + `MergeIter` (`src/engine.rs:439`,
  `:541`, `:592`) — forward-only, no per-source buffering, used by
  `query_events`, `resolve_position`, `cursor_for_position`,
  `summary::summarize`, and `bin/seeit.rs`.
- `Stepper` + `SourceWindow` + `pick` (`src/engine/merge.rs:395`,
  `:195`, `:601`) — bidirectional, per-source buffering, used by
  `StreamView` for the TUI.

Both implement: out-of-order detection (one-shot
`SourceError::OutOfOrder` warning), the "error head wins over event
head" rule, and tie-breaking by source-add order.  The two outputs
diverge in one annoying way:

| Concern             | `EventStream`    | `Stepper`     |
|---------------------|------------------|---------------|
| Output type         | `EngineEvent`    | `MergeRecord` |
| Has `LogStreamPosition`? | yes (computed in `SourceCursor::fill`) | no |
| Has byte offset/length? | no | yes |
| Has raw line? | no | yes |
| Tracks intra-time ordinal? | yes | no |

So `materialize_streamview` (`src/bin/seer.rs:789`) hand-recomputes
the per-time ordinal from the `MergeRecord` window because the
stepper threw the information away.  This is the worst kind of
duplication: the two paths know the same facts but expose them
differently, and a downstream consumer has to reconstruct what one
path already had.

**Fix sketch:** One merged record type that carries everything
(`source_id`, `offset`, `length`, `raw`, `event`, `position`).  The
forward-only `EventStream` becomes a thin wrapper around
`Stepper::step_forward` that adds the records-parsed counter.  The
out-of-order detection moves into `SourceWindow` (or whichever side
of the merge owns "we already saw this source's previous time").

This is the largest single cleanup in the library and would pay for
itself in test surface: today the two merges have separate tests
that lock in the same rules from different angles.

### A3 — Two `ParseStats` structs

(Restated from Phase 2 finding 6.)  `streamview::ParseStats`
(`src/streamview.rs:153`) is a strict superset of the binary's
`ParseStats` (`src/bin/seer.rs:683`).  Reduce to one type at the
library level and import it everywhere.

### A4 — `Tab` carrying parallel materializations of one window

(Restated from Phase 2 finding 5.)  `Tab` holds `streamview:
Option<StreamView>` *and* `events`/`formatted`/`event_for_line`/
`first_line_for_event` flat arrays produced by
`materialize_streamview`.  The arrays exist mostly because the
renderer wants flat `usize`-indexed access; the streamview wants a
deque so it can trim from either end.

Fix options, in order of intrusiveness:

1. **Cheapest:** make `materialize_streamview` an inherent method
   on `StreamView` that returns a borrowed view of the cached
   data, computed once per slide and cached on the streamview
   itself.  The Tab stops carrying the arrays.
2. **More invasive:** Have `StreamView` own the flat lines directly
   (rebuilt on every slide), and drop the deque-of-records shape.

The current "both" is the only wrong answer — every slide
materializes the same data twice.

### A5 — `LongOp` machinery in the binary

(Restated from Phase 2 finding 7.)  `LongOp` + `SummaryOp` +
`SearchOp` + `SeekOp` (`src/bin/seer.rs:939`–`:1240`) is engine-
shaped chunked-driver logic.  None of it imports ratatui.  Could
move into the library so `seeit --progress` (and future tests
covering the chunking semantics) can use the same machinery.

### A6 — `MergeError(Arc<SourceError>)`

`src/engine/merge.rs:148` wraps `SourceError` in `Arc` so the
stepper's buffers can hand out `Clone` copies cheaply (the
underlying `std::io::Error` / `serde_json::Error` aren't `Clone`).
The doc comment explains it well.

This is the right shape; flagging only because the wrapper exists
mostly to defeat the absence of `Clone` on standard errors.  If
parse errors were stored as their `Display` strings on the merged
record (the underlying line is already lost in `raw`), the `Arc`
indirection could go away.  Defensible either way.

### A7 — `Stepper::step_backward_n` without a forward sibling

`src/engine/merge.rs:529` is a small convenience that walks
backward `n` times and returns the records in forward order.  No
matching `step_forward_n`.  Asymmetric API surface.

Either drop the helper (call sites can loop on `step_backward`
themselves; the only in-tree caller is `seeit`'s `--before N`) or
add the forward variant.

### A8 — Five `extend_*` methods on `StreamView`

`extend_forward_batch`, `extend_forward_small_batch`,
`extend_backward_batch`, `extend_backward_small_batch`,
`extend_backward_batch_n` (`src/streamview.rs:809, :849, :900,
:905, :952`).  The `_small_batch` variants exist for the long-op
chunked path so the user-visible work per tick is bounded; the
`_batch_n` is for the seek-to-cursor's bounded back-fill.

The fan-out is real but the names obscure which one is the new
default and which is the legacy path.  Worth consolidating into one
or two methods parameterized by `(direction, batch_size,
max_walks)`.  Today, the constants `BATCH_SIZE = 64` and
`LONG_OP_BATCH_SIZE` (in merge.rs) plus the choice of method form
a three-axis matrix the reader has to keep in their head.

### A9 — `App`'s field-access helpers

`src/bin/seer.rs:2983` (`active_filter`), `:2991`
(`active_show_extras`), `:2999` (`active_show_date`), `:3007`
(`active_show_raw`), `:3014` (`active_hostname_display`) — each is
a one-line method that does `session.streams.get(&id).expect("stream
exists").FIELD`.  Five separate helpers replicating the same lookup.

A single `active_stream(&self) -> &LogStream` (or
`&mut LogStream`) accessor would let callers pluck whichever field
they need, and the bare `.expect("stream exists")` would live in
exactly one place — easier to audit.

### A10 — `Predicate` mixes event-level and source-level predicates

`Predicate::SourceIdMatches` (`src/filter.rs:133`) is the only
variant whose `matches(&self, event)` impl is a *lie* — it returns
`true` unconditionally because the engine has already excluded
non-matching sources from the merge before per-event matching runs.
The split is real (engine filters sources up front; the rest are
per-event), but the type pretends both kinds of predicate live on
the same dimension.

**Fix:** Two enums.

```rust
pub enum EventPredicate { LevelAtLeast(...), LevelEquals{...}, FieldEquals{...}, MsgMatches{...}, TimeBound{...} }
pub enum SourcePredicate { Matches{regex, negated} }
pub struct Filter {
    source_predicates: Vec<SourcePredicate>,
    event_predicates: Vec<EventPredicate>,
}
```

`Filter::matches` only sees event predicates; `Filter::matches_source_id`
only sees source predicates.  The "matches returns true" hack
disappears.  The DSL parser still accepts the same tokens — only the
internal representation changes.

This pairs naturally with the Polarity newtype suggested in Phase 3
(replacing `negated: bool`).

## B. Abstractions in the wrong place

(Cross-references to Phase 2.)

- **B1** — `Cursor` in `engine/merge.rs` is used by `session::Tab`
  and `session::Bookmark`; should live next to `ByteOffset` or in a
  top-level `position` module.  (Phase 2 finding 3.)
- **B2** — `LogStreamPosition` in `stream.rs` is only produced by
  `engine` and consumed by `session`/TUI; doesn't belong in the
  same file as `LogStream`.  (Phase 2 finding 4.)
- **B3** — `SessionId` in `session_store.rs` is logically a property
  of a `Session`.  (Phase 2 finding 2.)

## C. Missing abstractions

### C1 — `BATCH_SIZE = 64` repeated

(Phase 2 finding 6 / Phase 3 magic-literals finding.)  Two
`const BATCH_SIZE: usize = 64` declarations in `engine/merge.rs:48`
and `streamview.rs:39`.  One named constant in the library root
would resolve it.

### C2 — Index types

(Phase 3 newtype finding.)  `event_for_line: Vec<usize>` and
`first_line_for_event: Vec<usize>` are an accident waiting to
happen: same primitive type, inverted semantics.  Newtypes
`LineIdx` and `EventIdx` would catch the inevitable transposition
at compile time.

### C3 — `Direction` is underused

`src/source.rs:105` defines `Direction { Forward, Backward }`.  The
codebase consistently uses it inside `engine/merge.rs`, but
`StreamView` uses two separate `forward_eof: bool` /
`backward_eof: bool` fields (Phase 3 finding) and a separate
`TimeDir { Forward, Backward }` (`src/streamview.rs:200`) that
duplicates `Direction`.  Adopting `Direction` uniformly would
eliminate both repetitions.

### C4 — `LogStream` ↔ `RenderOpts` mirror

(Phase 3 blocking finding.)  Six fields duplicated, two methods
copying between them by name, no destructuring.  Compiler can't
help here today.  Either:

- merge: `LogStream` holds a single `render: RenderOpts` field
  (the persisted shape becomes a nested object — one schema bump
  with a migration shim), or
- enforce: destructure on both directions.

The persisted-shape argument has merit, but the destructure fix
is small and uncontroversial.

### C5 — Out-of-order detection in two places

Both `SourceCursor::fill` (`src/engine.rs:475`) and an equivalent
piece of the stepper detect timestamp regression and emit a
one-shot `OutOfOrder` warning per source.  If A2 lands (one merge
to rule them all), this collapses naturally.  Until then,
extracting a small helper struct (`OutOfOrderDetector`) that both
call sites use would eliminate the divergence risk.

## What looks well-shaped

- `RenderOpts` (apart from its mirror in `LogStream` and the
  `show_raw` overload) is a clean bundle: small, copy, defaults
  match a fresh stream.
- The `Source` trait surface is right-sized (id, metadata, events,
  query, byte_len, query_bounded).
- `SourceMetadata::excludes_all` is a small but meaningful
  optimization that lives in the right place — a future
  multi-million-line support tarball benefits the most from it.
- `summary` is well-factored: a streaming builder, a finalize step,
  a separate formatter.  The single-pass design is documented and
  honored.
- `save_policy` is genuinely small (218 lines, mostly tests) and
  well-isolated.  Easy to audit, no I/O, no clock — the right
  shape for a piece of bookkeeping logic.
- `seeit_target` is good: one struct that consolidates the
  reproduction-input shape, used in both directions.

## Summary

Three large pieces would together remove most of the duplication
in the library:

1. **Unify the two k-way merges (A2)** — biggest single win;
   eliminates the divergent record types and the
   `materialize_streamview` ordinal recomputation.
2. **Consolidate stepper constructors (A1)** and the streamview
   `extend_*` family (A8) — same shape, smaller scope.
3. **Resolve the `Tab` / `StreamView` materialization (A4)** —
   either move materialization onto `StreamView` or drop the
   parallel arrays from `Tab`.

The smaller findings (A3, A5, A6, A7, A9, A10, plus the C-list)
are mechanical refactors that can land independently.
