# Distinct operations the merged-event iterator must support

Working from what the existing code already does (`StreamView`,
`summary::summarize`, `SearchOp`, `SeekOp`, `SummaryOp`,
`cursor_for_position`), the distinct operations split into roughly
three groups.

## Iteration primitives

1. **Step forward** — return next event after the current position.
2. **Step backward** — return previous event before the current
   position.  Symmetric with (1); the merge has to maintain reverse
   lookbehind.
3. **Bounded step** — same as (1)/(2) but capped at *N* records walked
   or *M* bytes scanned, so a single tick can return "budget exhausted,
   here's where I got to" rather than freezing the UI under a selective
   filter.

## Positioning

4. **Start at a cursor** — physical position (per-source byte offsets).
   This is the bookmark-seek primitive and the natural unit of
   persistence.  `Bookmark` stores a `Cursor` directly; no callsite in
   the tree starts iteration from a `LogStreamPosition`.
   (`Engine::cursor_for_position` exists but has no live callers and
   should be removed.)
5. **Snapshot current cursor** — hand back a cursor for persistence or
   to spawn another iterator on the same spot (used when reconstructing
   a stepper per long-op tick).
6. **Advance to a wall-clock time** — jump forward or backward to the
   first event at-or-after / at-or-before a target `DateTime`.  Today
   this is `<` / `>` on top of `step_forward`/`step_backward`, but it's
   a distinct conceptual op.

## Filter and lifecycle

7. **Swap the event-level filter in place** — retain position, change
   which events pass.  Source-set filter changes require a fresh
   iterator.
8. **Exhaustion check per direction** — distinguish "ran out of budget"
   from "true end of stream", needed to terminate scrolling and
   searches correctly.
9. **Search step** — repeated step + predicate match until hit, with
   bounded variant for long-op chunking.  Arguably composed of (3) +
   a predicate, but it's the operation the UI thinks in.
10. **Full-stream fold** — drive an iterator to EOF without budget,
    for summary and count.  Single forward pass; no buffering needed
    beyond what step provides.

## Telemetry

11. **Bytes / records walked since construction** — progress
    reporting.  Currently lives on both `EventStream` (`bytes_read`,
    `records_parsed`) and `Stepper` (`walked_bytes`); the TODO calls
    out adding `records_parsed`/`bytes_read` on `Stepper` so the
    merge is the single source of truth.

## Observation

One thing worth naming explicitly: (3), (6), (9), and (10) are all
variations on stepping with different stop conditions (budget, time
target, predicate match, EOF).  A sound abstraction would express the
budget and stop condition as parameters of a single step call rather
than four separate methods.  That would also be where `EventStream`
finally disappears — it's just "step with EOF as the stop condition,
no budget" today.
