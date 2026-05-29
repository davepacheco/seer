# Cheap `Stepper` clone

## Goal

Make `Stepper` cheaply cloneable so multiple steppers can share cached
buffered records but step (and filter) independently.  Motivating use
case: prototype a `StreamView`-like structure that holds multiple
`Stepper`s rooted at the same engine.

## End state

- `Source: Send + Sync`; sources are handed out as `Arc<dyn Source>`.
- `Stepper` and `SourceWindow` have no `'a` lifetime parameter.
- Per-source buffers hold `Arc<BufferedRecord>` rather than owned
  `BufferedRecord`.
- `MergeRecord` is roughly `(Arc<dyn Source>, Arc<BufferedRecord>)`
  with accessor methods (`source_id()`, `offset()`, `length()`,
  `event()`, `raw()`) in place of `pub` fields.
- `Stepper`, `SourceWindow`, and `BufferedRecord` all derive `Clone`.
- Cloning a `Stepper` `Arc::clone`s the per-source buffer entries; the
  clone has its own per-source `position`, EOF flags, and `Filter`,
  so stepping or filtering on one copy does not affect the other.

## Side benefit

Removing the `'a` from `Stepper` also unblocks the `SummaryOp`
follow-up noted in `CLAUDE.md` (a long-lived `Stepper` across long-op
ticks instead of reconstruction from `Cursor` each tick).  That is a
separate task — do not bundle it in.

## Steps

Each step compiles and tests on its own.  Stop or land between any
pair.

- [x] **1. Establish `Source: Send + Sync`.**  Add the bounds.  Verify
      `FileSource` qualifies — `File`, `BTreeMap`, `String` all do; the
      check is mostly that no `Rc`, `Cell`, or `RefCell` snuck in.  No
      behavioral change.  Self-contained commit so any fallout is
      isolated.

- [ ] **2. Hand sources out as `Arc<dyn Source>`.**  Change `Engine`'s
      source storage to `Vec<Arc<dyn Source>>`.  Update
      `Engine::stepper` / `Engine::stepper_with` to return `Stepper`
      (no lifetime parameter) and to clone the `Arc`s into the
      stepper.  Touches every `Stepper` construction site: `seer.rs`,
      `seeit.rs`, `streamview.rs`, plus the tests in
      `engine/merge.rs` and `engine.rs`.

- [ ] **3. Drop the lifetime from `Stepper` and `SourceWindow`.**
      Replace `source: &'a dyn Source` with
      `source: Arc<dyn Source>`, delete the `'a` parameter, and
      remove the cached `source_id` field on `SourceWindow` —
      `source.id()` provides it.  Mostly mechanical once step 2 is in.

- [ ] **4. Migrate `MergeRecord` to getters and an `Arc<dyn Source>`.**
      Replace the `pub` fields with accessor methods:
      - `source_id(&self) -> &SourceId` (routes through the stored
        `Arc<dyn Source>`)
      - `offset(&self) -> ByteOffset`
      - `length(&self) -> ByteLen`
      - `event(&self) -> &Result<Event, MergeError>`
      - `raw(&self) -> &str`

      `MergeRecord` itself stores `Arc<dyn Source>` plus the existing
      owned `event` / `offset` / `length` / `raw` fields for now (the
      `Arc<BufferedRecord>` rewrite happens in step 5).  Migrate
      every call site (`engine.rs`, `streamview.rs`, `seeit.rs`, the
      merge tests).  Tests that did `r.event.unwrap()` become
      `r.event().as_ref().unwrap()`.  Big mechanical diff, no
      behavior change.

      May be combined with step 5 if the field-to-getter churn feels
      wasteful to spread across two passes.  Splitting keeps each
      diff smaller and separates API migration from the
      perf-affecting representation change.

- [ ] **5. Share the buffered entries via `Arc<BufferedRecord>`.**
      Change `SourceWindow`'s two `VecDeque<BufferedRecord>` to
      `VecDeque<Arc<BufferedRecord>>`.  `fill()` wraps each
      `QueryRecord` in an `Arc`.  `pop()` becomes `Arc::clone` into
      the opposite buffer (no more deep clone of `Event` + `raw`)
      and constructs `MergeRecord` from
      `(Arc::clone(&self.source), Arc::clone(&head))`.  The
      accessor signatures from step 4 do not change; only the
      internal representation of `MergeRecord` does.

- [ ] **6. Derive `Clone` on `Stepper`, `SourceWindow`,
      `BufferedRecord`.**  At this point every field is cheaply
      cloneable (`Arc`s, scalars, the existing `Filter: Clone`).
      Add a unit test that:
      - clones a `Stepper`,
      - advances one copy past several records,
      - calls `set_filter` on the same copy,
      - verifies the other copy still walks from its prior position
        with its prior filter, exercising the shared cache via
        `Arc::clone`s rather than re-fetching.

- [ ] **7. Prototype the multi-stepper `StreamView` variant.**  With
      cheap clone in hand, sketch the multi-stepper version.  Shape
      TBD; keep it separate from the existing `StreamView` until the
      design settles.

## Decisions to confirm up front

- **Step 4 / 5 combination.**  Land separately (smaller diffs,
  cleaner review) vs. combined (one round of call-site touch).
  Either is fine.

- **`cursor()` semantics under clone.**  After cloning, each
  `Stepper` has its own per-source `position`, so `stepper.cursor()`
  reflects that copy's progress.  Add a one-line rustdoc note and
  cover it in the clone test.

- **EOF flags.**  Per-`SourceWindow` and so per-clone — one stepper
  hitting EOF must not bias the other.  No action, just confirm in
  the clone test.

## Things worth keeping an eye on

- **Per-entry allocation.**  `Arc<BufferedRecord>` adds one
  allocation per buffered record.  With `BUFFER_LIMIT = 256` per
  direction per source, this is bounded and small, but it is a
  change from the inline `VecDeque` slot.

- **`Source: Send + Sync` propagation.**  Any test helper or wrapper
  source (e.g. `CountingSource` in `engine/merge.rs` tests) must
  also satisfy the new bounds.
