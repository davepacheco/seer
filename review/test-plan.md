# Plan: scale tests and benchmarks

## Status

| Field         | Value                                                |
|---------------|------------------------------------------------------|
| Current phase | Phase 0 — not yet started                            |
| Next step     | Phase 0, step 1 (add `test-fixtures` feature flag)   |
| Last updated  | 2026-05-12                                           |
| Notes         | —                                                    |

### How this state works

This file is the canonical record of where the plan stands.  Each
actionable item below is a Markdown checkbox: `- [ ]` for pending,
`- [x]` for complete.  When asked to "take the next step":

1. Read the **Status** table above.
2. Find the first unchecked box in the listed phase.
3. Execute that step (and only that step unless the user asks for
   more).  Updating tests, running `cargo check`/`cargo nextest`, and
   verifying compilation are part of the step — don't claim a step
   done if the build is broken.
4. Tick the checkbox.  Update the **Current phase**, **Next step**,
   and **Last updated** rows.  Use **Notes** for any caveat the
   user should know about (e.g. "skipped test 7 because the
   `OutOfOrder` warning location is being reworked in another
   PR").
5. If the step revealed a new sub-task that wasn't in the plan,
   add it as a checkbox and tick the original step — don't
   silently absorb scope.

A new conversation can pick up exactly where the last one left off
by reading this block.

---

A plan to close the two gaps from Phase 5's end-of-review summary:

1. **Scale fixtures** — single-source files with tens of thousands of
   records, and multi-source bundles, so window trims, refetches,
   long-op chunking, and merge-many-sources paths are exercised at
   sizes representative of real Oxide support data.
2. **Benchmark suite** — criterion benches on the engine hot paths
   so a refactor can demonstrate "no regression" rather than only
   "passes correctness tests".

The plan lands in phases that build on each other but can be paced
independently.

## Phase 0 — Shared fixture infrastructure

The existing helpers (`src/test_util.rs`) are gated on `#[cfg(test)]`
so they're invisible to integration tests under `tests/` and to
benchmark crates under `benches/`.  Both new bodies of code need
fixture-generation helpers.

**Approach:** expose a `pub mod test_fixtures` behind an opt-in
feature.

```toml
# Cargo.toml
[features]
test-fixtures = []
```

```rust
// src/lib.rs
#[cfg(any(test, feature = "test-fixtures"))]
pub mod test_fixtures;
```

`src/test_fixtures.rs` exports:

- `TestDir` — moved or re-exported from `test_util`.
- `gen_single_source(path, name: &str, count: usize, opts: &GenOpts)`
  — writes `count` slog-bunyan records to `path` with deterministic
  timestamps (`base + i * step`), every Nth record carrying extra
  fields so summary tests have something to histogram.  `GenOpts`
  controls the level mix, the message templates, and the rate of
  extras.
- `gen_multi_source(dir, count_per_source, source_names)` — writes
  one file per source with interleaved timestamps so the merge has
  real work to do.
- `gen_with_parse_errors(path, count, error_every: usize)` — for
  error-handling at scale.

Targets to land before the next phase:

- [ ] **0.1** Add `[features] test-fixtures = []` to `Cargo.toml`.
- [ ] **0.2** Create `src/test_fixtures.rs` (file-level emitters,
      `TestDir`, `GenOpts`, `gen_single_source`,
      `gen_multi_source`, `gen_with_parse_errors`).  Re-export
      under `#[cfg(any(test, feature = "test-fixtures"))]` in
      `lib.rs`.
- [ ] **0.3** Move (or re-export) the existing helpers from
      `src/test_util.rs` into the new module.  Adjust call sites
      in unit tests so they import from the new path.
- [ ] **0.4** Verify both flavors compile and pass:
      `cargo check --all-targets` and `cargo check --all-targets
      --features test-fixtures`.
- [ ] **0.5** Run `cargo nextest run` on the affected packages to
      confirm no test regressions from the helper move.

## Phase 1 — Scale integration tests

A new `tests/scale.rs` adding sub-second tests that exercise the
window-management and merge code on fixtures of ~50K records (one
file ~5 MB).  This is the size at which `BUFFER_LIMIT = 256`,
`BATCH_SIZE = 64`, and `WINDOW_SOFT_CAP = 1024` all come into play
multiple times.

`tests/scale.rs` would import the new feature:

```toml
# Cargo.toml
[[test]]
name = "scale"
required-features = ["test-fixtures"]
```

### Tests to land

- [ ] **1.0** Add the `[[test]] name = "scale"` entry to `Cargo.toml`
      with `required-features = ["test-fixtures"]`, and an empty
      `tests/scale.rs` so the harness wires up before any test
      lands.

- [ ] **1.1** `streamview_forward_scrolls_through_50k_records`
   - Build a 50K-record source.  Construct a `StreamView` with the
     default filter.  Call `ensure_window`, then `scroll_lines(+500)`
     forty times.  Assert: the anchor advances monotonically, the
     viewport never moves backward, no record is skipped or
     duplicated relative to a parallel `EventStream` walk of the
     same source.

- [ ] **1.2** `streamview_backward_after_full_forward_replays_in_reverse`
   - Walk forward to the end (forces several trims of the backward
     buffer).  Walk backward to byte 0.  Assert the reversed
     backward sequence equals the forward sequence — proves the
     `pop` mirror logic and the EOF-clearing-on-mirror invariant.

- [ ] **1.3** `set_filter_after_full_walk_drops_buffers_and_resets`
   - Walk forward through 10K records.  `view.set_filter(...)` to a
     filter rejecting 99.5% of records.  `ensure_window`.  Assert:
     records.len() reflects the new filter, parse_stats reset,
     anchor is `PinFront`.

- [ ] **1.4** `multi_source_merge_emits_in_time_order_across_ten_sources`
   - Ten 5K-record sources with overlapping timestamps.  Build an
     `Engine`, run `query_events` to completion.  Assert: events
     emerge in non-decreasing `time` order; no source's records are
     skipped.

- [ ] **1.5** `multi_source_merge_under_selective_filter_walks_each_file`
   - Same fixture; filter that accepts ~1 in 200 records.  Assert
     correctness *and* assert each source contributed at least one
     record (proves no source was silently pruned by the
     source-id-filter shortcut when it shouldn't have been).

- [ ] **1.6** `search_step_forward_finds_match_at_record_45000`
   - 50K-record source; one record at index 45000 has a unique
     phrase in its msg.  Search step forward.  Assert `Found` and
     anchor lands on that record.  Also assert the budget-exhausted
     resume path runs (the 50K records exceed `SEARCH_BUDGET =
     50_000`).

- [ ] **1.7** `summary_build_via_stepper_matches_eager_summarize`
   - 50K records.  Compute the summary two ways: via
     `summary::summarize(&engine, &filter)` and via a `SummaryOp`
     stepping through manually in chunks.  Assert the two
     `Summary` values are equal.  This is the test that pins down
     the long-op driver's correctness — currently nothing verifies
     it produces the same answer as the eager path.

- [ ] **1.8** `seek_to_cursor_under_selective_filter_yields_correct_anchor`
   - Anchor a `LogStreamPosition` at index 30000 in a 50K-record
     file.  Apply a filter excluding the bookmarked event itself
     but keeping everything around it.  Call `seek_to_cursor` and
     assert the resulting view's anchor is the next-visible record
     after the bookmark.

- [ ] **1.9** `cancel_seek_midstream_leaves_partial_window_in_consistent_state`
   - Start a `SeekOp` (or call the streamview-level equivalent),
     advance it a few chunks, then drop it.  Assert: a fresh
     stepper from the streamview's cursor produces a sensible
     forward walk (no record duplicated, no record missed).

- [ ] **1.10** `out_of_order_warning_emitted_once_across_a_50k_pass`
    - 50K records with one timestamp regression at index 25000.
      Walk the merge.  Assert exactly one `OutOfOrder` warning
      surfaces, at the right location.

### Sizing knobs

- 50K records of slog-bunyan output is ~5 MB.  Parsing the whole
  file takes ~1 s on a modern laptop, so each scale test fits well
  inside CI's per-test budget.
- For tests where the *visible* assertion needs only a few records
  (cancel-midstream, out-of-order), the fixture can shrink to 10K
  without losing coverage of the trim path.

## Phase 2 — Criterion benchmark suite

Add criterion as a dev-dependency and a `benches/` directory.

```toml
# Cargo.toml
[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }

[[bench]]
name = "engine"
harness = false
required-features = ["test-fixtures"]
```

### Bench targets

Each bench builds its fixture once (criterion's `setup` closure) and
runs the body multiple times.

- [ ] **2.0** Add `criterion` to `dev-dependencies` and create
      `benches/engine.rs` with one minimal benchmark group wired up
      so the harness builds before the real benches land.
- [ ] **2.1** `source_query_forward_unfiltered` —
      `FileSource::query_bounded` over a 50K-record file with
      `Filter::default()`.  Establishes the baseline parse
      throughput.
- [ ] **2.2** `source_query_forward_selective_1_in_100` — same,
      with a filter accepting ~1% of records.  Establishes the
      walk-but-skip throughput.
- [ ] **2.3** `source_query_backward_unfiltered` —
      `Direction::Backward`, start at EOF.  Exercises
      `read_record_before`'s chunked back-scan.
- [ ] **2.4** `stepper_forward_drains_50k_unfiltered` — `Stepper`
      built from a single source, repeatedly `step_forward` until
      `None`.  Difference from `source_query_forward` is the merge
      plumbing.
- [ ] **2.5** `stepper_forward_drains_10x5k_multi_source` — same,
      but ten sources.  Establishes the multi-source-merge
      throughput.
- [ ] **2.6** `stepper_step_forward_then_backward_one_each` —
      start at byte 0, `step_forward`, then `step_backward`.
      Exercises the buffer mirror without a refill.
      Microbenchmark for the lookbehind cache.
- [ ] **2.7** `stepper_set_filter_then_step_forward` — full filter
      rebuild on a populated stepper, then walk forward.
      Exercises `clear_buffers` plus a fresh fill.
- [ ] **2.8** `streamview_ensure_window_default_filter` —
      `StreamView::new` + `ensure_window(viewport=80)`.  The
      operation a fresh tab does on every filter change; matters
      for perceived UI responsiveness.
- [ ] **2.9** `streamview_scroll_lines_past_window_edge` —
      `scroll_lines(+200)` against a 1024-record window built from
      a 50K file.  Exercises the slide+trim path.
- [ ] **2.10** `summarize_50k_unfiltered` — `summary::summarize`
      over a 50K-record source.  Single number characterizing the
      summary build cost.

### Wiring

- `benches/common.rs` re-exports `seer::test_fixtures` so each bench
  doesn't have to write its own generator.
- One `criterion_group!` per file; one `criterion_main!` at the
  bottom.

### Running

- `cargo bench` runs every group.
- Criterion's HTML output lands under `target/criterion/`.
- For "did this PR regress anything?": criterion's built-in
  comparison against the previous run is sufficient — no separate
  harness is needed.

## Phase 3 — CI integration

Two decisions, both deferrable:

- [ ] **3.1** Add `cargo test --features test-fixtures --test
      scale` to the CI workflow.  Sub-second each; worth running.
- [ ] **3.2** Add `cargo bench --features test-fixtures --no-run`
      to CI so bench compilation breakage is caught.  Running the
      benches themselves only makes sense if a stable runner is
      available — defer that decision until one exists.

## Estimated effort

- **Phase 0** (fixture infrastructure): ~150 lines of code,
  half a day of work including porting `test_util.rs`.
- **Phase 1** (10 scale tests): roughly half a day per cluster.
  Tests 1-3 are one cluster, 4-5 another, 6-7 another, 8-10
  individually.  Total ~3 days.
- **Phase 2** (10 benches): ~1 day, mostly mechanical once the
  fixtures exist.
- **Phase 3** (CI): under an hour.

## What this plan does *not* try to cover

- **TUI rendering at scale.**  `TestBackend` painting tests cover
  small layouts well; visual regressions at large window sizes are
  out of scope.  If they become a concern, snapshot tests via
  `expectorate` (already a dev-dep) on a few specific render
  outputs would be the right shape.
- **Real-world log files.**  `t/sled-01..03.log` exist but aren't
  wired into a test today.  Worth adding a "smoke test against
  these checked-in samples" once the synthetic-fixture tests are
  stable, just to catch surprises in real-world format quirks.
- **Concurrency / event-loop scheduling.**  The codebase is
  single-threaded inside the event loop; the long-op driver
  interleaves with user input.  Tests for that interleaving live
  best in `bin/seer.rs` (existing patterns), not under `tests/`.

## Suggested landing order

1. Phase 0.  Validate the feature-gated module compiles and the
   existing tests still pass.
2. Phase 1, tests 1-3.  One PR; proves the infrastructure carries
   real coverage.
3. Phase 1, tests 4-10 in two or three PRs.
4. Phase 2 with one or two seed benchmarks.  Establishes the
   harness.
5. Phase 2, remaining benchmarks.
6. Phase 3.
