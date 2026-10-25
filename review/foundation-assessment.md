# Assessment of the current code as a foundation

Written 2026-10-24, against commit `3c78fba` on `prehistory-stepper-work`.

This note records what I found when I returned to the project after several
months.  It covers the state of the branch, a review of the lower layers and
the new `Viewport`, and a recommendation about whether to continue or start
over.

## Summary

The storage layer, the filters, and the merge stepper are a sound foundation.
There is one important exception: a `Cursor` is only meaningful under the
filter that produced it.  That flaw affects filter changes, bookmarks, and the
`Viewport`, and it should be fixed before more work goes into the layers above.

The `Viewport` design (a speculative seek on a cloned stepper, plus
incremental population of the window) is sound.  The implementation is an
early prototype.  It would be reasonable to rebuild it from the design once
the meaning of a position in a log stream is settled.

A proposed fix for the `Cursor` problem appears below, under "Proposal:
positions that do not depend on the filter."

The recommendation is to keep the engine and storage layers, write a spec, and
then rebuild `Viewport` and the TUI (`bin/seer.rs`) against that spec.

## State of the branch

- The branch is linear on top of `main` (51 commits ahead) and `prehistory`
  (30 commits ahead).  It matches `origin`.  The last commit is from
  2026-06-03.
- The work in progress is replacing `StreamView` with `Viewport`.  Both live
  in `src/streamview.rs`.  The TUI no longer uses `StreamView`.  It remains
  only in its own tests, the benchmarks, and doc comments.
- `cargo build --bins` succeeds.  The `seer` binary has 10 dead-code warnings,
  mostly from the old `LongOp` code (`SearchOp`, `SeekOp`, `SeekFinalize`,
  `progress_bar_inner`).
- The `seer` binary's unit tests do not compile.  There are 54 errors.  Nearly
  all of them come from tests that still refer to `App.long_op`,
  `drain_long_op()`, `LongOp`, and `Tab.streamview`.
- All other tests pass: 316 tests across the library, `seeit`, and the
  integration tests.
- There are 42 `XXX-dap` markers, mostly in `src/streamview.rs` and
  `src/bin/seer.rs`.

## Storage layer (`src/source.rs`)

The `Source` trait is small and clean.  It reads records forward or backward
from a byte offset, with an optional limit on how many records to scan.  That
is what the layers above need, and it should extend to other kinds of sources.

These gaps should be planned for.  None of them requires a redesign.

- **Lines that are not bunyan JSON become parse errors.**  The merge always
  emits an error ahead of any event, regardless of time (see `pick()` in
  `src/engine/merge.rs`).  In a merged view, SMF lines from one file will
  therefore appear in the wrong place.  SMF is the most important format after
  bunyan, so this needs a real design.
- **The storage layer assumes that each record is one line.**  `scan_forward`
  reads one line at a time, and `read_record_before` searches backward for the
  previous newline.  Other formats, such as CockroachDB logs, have records
  that span several lines.  Each format will need to say where its records
  begin.  `spec/core-model.md` describes one way to do that.
- **`Event` has the bunyan fields built into it.**  CockroachDB logs and
  syslog will need a more general record type.
- **Seeking by time is a linear scan.**  Each file is sorted by time, so a
  binary search over byte offsets could find a time with a handful of reads.
  This would be a new method on `Source`.
- **Time ranges do not skip whole files yet.**  The module comment says they
  do, but `SourceMetadata::excludes_all` only checks `name` and `hostname`.
  A `time>=X` filter scans each file from the beginning.
- **Compressed files and tarballs have no random access.**  They will need to
  be decompressed to a temporary file or cached.
- **Every query reopens the file.**  This is acceptable for now.  It may be
  worth revisiting along with the backward-read path, which reads each record
  with at least two seeks.

## Filters (`src/filter.rs`)

A filter is a flat list of conditions that must all hold.  This is simple and
sound.  Named filter groups, and filters like "show A or B", will eventually
need a tree of conditions.  That change can be made gradually from here.

## Merge stepper (`src/engine/merge.rs`)

The k-way merge with lookahead and lookbehind buffers is good work.  It is
well tested and handles a limited scan budget carefully.  Sharing records
through `Arc<BufferedRecord>` makes a cloned `Stepper` cheap.

### Design flaw: a `Cursor` depends on the filter

A `Cursor` stores one byte offset per source.  Those offsets are only
consistent with each other under the filter that produced them.

There are two causes:

1. A source whose next match is still sitting in a buffer keeps its old
   offset, even if the stepper has already read and rejected records after
   it.  Under a broader filter, the records between that offset and the
   anchor become visible, and they appear *after* the anchor even though they
   are earlier in time.
2. Under a scan budget, `SourceWindow::fill` moves a source's offset past a
   stretch of rejected records when a batch returns no matches.  Stepping back
   does not undo that move.  The cursor that results from a walk therefore
   depends on the path taken, not only on where the walk ended.

#### Reproduction

I confirmed the first cause with a temporary integration test, which I have
since deleted.  It used two sources:

- Source A: `a1-noise` (t=1), `a2-noise` (t=2), `a10-match` (t=10).
- Source B: `b3-match` (t=3), `b4-match` (t=4).

With the filter `msg=~match`, one `step_forward` lands on `b3-match`.  The
resulting cursor has A at offset 0 and B at offset 114.  Reusing that cursor
with no filter, which is what `Viewport::set_filter` does, gives this:

- stepping backward: `["b3-match"]`
- stepping forward: `["a1-noise", "a2-noise", "b4-match", "a10-match"]`

The records at t=1 and t=2 appear after the anchor at t=3.

The test was:

```rust
use seer::test_fixtures::{TestDir, append_bunyan_at, t};
use seer::{Cursor, Engine, Filter};

fn msgs(it: impl Iterator<Item = seer::MergeRecord>) -> Vec<String> {
    it.filter_map(|r| r.event().as_ref().ok().map(|e| e.msg.clone()))
        .collect()
}

#[test]
fn cursor_reused_under_broader_filter() {
    let dir = TestDir::new();
    let a = dir.path().join("a.log");
    let b = dir.path().join("b.log");
    append_bunyan_at(&a, "A", t(1), "a1-noise");
    append_bunyan_at(&a, "A", t(2), "a2-noise");
    append_bunyan_at(&a, "A", t(10), "a10-match");
    append_bunyan_at(&b, "B", t(3), "b3-match");
    append_bunyan_at(&b, "B", t(4), "b4-match");
    let mut engine = Engine::new();
    engine.add_file_source(&a).unwrap();
    engine.add_file_source(&b).unwrap();

    let narrow: Filter = "msg=~match".parse().unwrap();
    let mut s = engine.stepper(narrow, &Cursor::new());
    let _anchor = s.step_forward().unwrap();
    let cursor = s.cursor();

    let mut fwd = engine.stepper(Filter::default(), &cursor);
    let mut back = engine.stepper(Filter::default(), &cursor);
    let before = msgs(std::iter::from_fn(|| back.step_backward()));
    let after = msgs(std::iter::from_fn(|| fwd.step_forward()));
    println!("before (nearest first): {before:?}");
    println!("after: {after:?}");
}
```

#### Consequences

- **Changing the filter** can show records out of time order.
  `Stepper::set_filter` and `Viewport::set_filter` both keep the old offsets.
- **Bookmarks** store a `Cursor`.  A bookmark taken under one filter can
  misbehave when it is opened under another.
- **`RenderedWindow::stepper_at_cursor_within_window`** walks a cloned stepper
  until its cursor is *equal* to the anchor's cursor.  It panics if it never
  finds one, or if the two cursors cannot be ordered.  The second cause above
  makes an exact match unreliable.  I suspect this explains the TODO item
  "doing a filter and then navigating immediately crashes", but I have not
  reproduced that crash.

#### Why rejected records do not advance the offset

In the example, the stepper does read and reject `a1-noise` and `a2-noise`.
`SourceWindow::fill` asks the storage layer for a batch of matches starting at
A's offset of 0.  The storage layer rejects both noise records and returns a
batch that contains only `a10-match`.  Because the batch is not empty, `fill`
leaves A's offset at 0 and puts `a10-match` in A's lookahead buffer.  A's
offset only moves when one of A's records is emitted (`SourceWindow::pop`).
The merge emits `b3-match` first, so A is still at 0 when the cursor is
saved.

This also means that a documented invariant is wrong.  The comment on
`SourceWindow::forward_buf` says that, when the buffer is not empty, the
offset of its first record equals `position`.  In this example, the buffer's
first record is `a10-match` at about byte 228, while `position` is 0.

#### Why advancing the offset would not fix it

Suppose `fill` moved A's offset up to the first match it found.  That would
fix the example above, but it would only move the error to the other side of
the anchor.  If A's hidden record were at t=5 instead of t=1, it would end up
behind the cursor.  Under no filter, it would then appear *before* `b3-match`
when stepping backward, even though it is later in time.

The underlying problem is that, whenever the merge emits a record, every
other source's offset should mark the first record after that point in time.
That must hold for every record in the file, not only for records that pass
the filter.  The stepper never learns that offset.  It only knows how far it
has scanned each source while looking for matches, and that distance depends
on the filter and on the batch sizes.

The next section proposes a fix.

## Proposal: positions that do not depend on the filter

The proposal separates two ideas that `Cursor` currently combines.  The first
is the user's position in the stream.  That should be independent of the
filter and safe to save.  The second is how far the stepper has read each
source.  That is internal state tied to the filter, and it should never leave
the stepper.

### 1. One total order over all records

Every record gets a sort key that does not depend on any filter:

```rust
/// Sort key for a record in the unfiltered merged stream.
struct RecordKey {
    time: DateTime<Utc>,
    source_id: SourceId,
    offset: ByteOffset,
}
```

Records are ordered by time first, then by source, then by offset.  This
order is the merged stream with no filter applied.  A filtered view is a
subsequence of it.

The tiebreak between sources should use `SourceId` rather than the order in
which sources were added.  Today the merge breaks ties by index in the source
list.  That index can change when the user adds or removes sources, and a
saved position would then quietly change its meaning.

Records that fail to parse also need a time.  Each one could take the time of
the preceding parsed record in its file.  That is also a reasonable first
answer for SMF lines.  They would stay next to the bunyan record they
followed, instead of jumping to the front of the merge as they do today.

### 2. A position is a point between two records

```rust
enum StreamPosition {
    Start,
    /// just before the record with this key
    Before(RecordKey),
    End,
}
```

Because `RecordKey` does not depend on the filter, a `StreamPosition` splits
every filtered view at the same point in time.  If a new filter hides the
record named in the key, the position still makes sense, since it falls
between the record's visible neighbors.  Bookmarks, sessions, and the
`Viewport`'s anchor would store this type.

`Start` and `End` replace `Cursor::new()` and `Engine::cursor_at_end()`.  They
are explicit cases rather than special cursor values.

### 3. The stepper builds its offsets from a position

To build a stepper at `Before(key)`, each source needs its own boundary
offset:

- For the source that holds the key's record, the boundary is that record's
  offset.
- For every other source, the boundary is the offset of the first record that
  sorts after the key.  Every record counts here, not only the ones the filter
  accepts.

Each file is sorted by time, so a binary search over byte offsets can find
that boundary.  The search seeks to a midpoint, skips forward to the next
line, reads that line's time, and repeats.  That takes about 20 to 30 small
reads for a large file.  A short scan forward from the result then settles
ties between records that share a timestamp.  This would be a new method on
`Source`.  Seeking by time (`<`, `>`, and jumping to a time) would use the
same method.

After that, the stepper works the way it does today.  Its per-source offsets
and buffers become a private cache that is only valid under the current
filter.  The problem described above no longer matters, because those offsets
are never saved or reused under another filter.

### 4. The stepper reports a position, not offsets

`Stepper::cursor()` would be replaced by something like
`Stepper::position()`.  It returns `Before(key)`, where the key belongs to the
record the next `step_forward` would return.  That record is either already in
a buffer, or the stepper is at the end, so the key is easy to compute.

Changing the filter becomes simple.  The caller takes the current position,
builds a new stepper from it under the new filter, and drops the old stepper.
`Stepper::set_filter` and its behavior of keeping the old offsets can be
removed.

### 5. The `Viewport` compares records, not cursors

`stepper_at_cursor_within_window` currently walks until two cursors are
equal, and `Cursor` has only a partial order.  With record keys, the walk
instead compares the key of each record it steps over with the anchor's key.
Keys are totally ordered, so the direction of the walk is always clear.  The
walk can also stop as soon as it passes the anchor, rather than panicking.
The same approach covers `cursor_before_record()`, because the position
before record R is `Before(R's key)`.

### Open questions

- **Sources that are out of order.**  Binary search assumes a file is sorted
  by time.  For a file that is not, the search can return a wrong boundary.
  The engine could detect this when it first reads the file and fall back to
  a linear scan for that source.  The TODO list in `CLAUDE.md` already says
  the engine should check this.
- **What the anchor does when its record is hidden.**  If a filter change
  hides the anchor record, the view could move to the nearest visible record
  after the position.  It would fall back to the record before when nothing
  follows.  The right policy is a UX choice.
- **Migrating sessions.**  Bookmarks and tabs store a `Cursor` today.
  Converting one to a `StreamPosition` only works reliably under the filter
  that was active when it was saved.  One approach is to rebuild a stepper
  with that filter and take the key of its next record.  Another is to drop
  old bookmarks.  This needs a decision before the session format changes.
- **What counts as a record for a multi-line entry.**  If plain-text lines are
  later folded into the bunyan record before them, the key would refer to the
  first line's offset.  That works, but it should be decided together with
  the SMF design.

### What else this resolves

- It answers the question in `review/dap-review.txt` about how `Cursor` and
  `LogStreamPosition` differ.  `Cursor` is a merge frontier that depends on
  the filter, and `LogStreamPosition` is closer to an identity for a record.
  This design replaces both with `RecordKey` and `StreamPosition`.
- It makes seeking by time fast.
- It removes the panic paths in `Viewport` that depend on cursor equality.

## `Viewport` (`src/streamview.rs`)

The design matches the notes in `review/streamview.md` and seems sound:

- A seek runs on a cloned stepper, so ^C simply drops the clone and leaves the
  current window alone.
- Filling the window happens a little at a time.
- The TUI drives seeking and filling in a loop with a time limit
  (`App::do_work` in `src/bin/seer.rs`).

The implementation is still early:

- **Every scroll past a record boundary rebuilds the rendered window.**
  `seek_finish` always creates a new `RenderedWindow` with zero records before
  the anchor and 1024 after it.  Each `j` or `k` that crosses into another
  record clones the stepper, walks it back to the anchor, and starts rendering
  again.  The notes describe a layer that would avoid this.  In that design,
  the viewport moves within a cached window and seeks only near its edges.
  That layer has not been built.
- **There are crash paths.**  One is the cursor-equality walk described above.
  Others are the `expect` in `seek_finish` and `cursor_before_record()`, which
  is still a `todo!()`.  Bookmark creation needs `cursor_before_record()`.
- **Tiebreakers depend on the window.**  `RenderedWindow::render` numbers
  records that share a timestamp starting from the top of the window.  The
  same record can therefore get a different `LogStreamPosition` depending on
  where the window starts.
- **Some cases are unhandled.**  The `XXX-dap` notes mention the anchor being
  pinned to the back and a filter change hiding the anchor record.
- **`populate_work` renders one record per call.**  This is fine under the
  time-limited loop, but each call's cost varies widely under a selective
  filter.

## Recommendation

Keep these:

- `source.rs`, `filter.rs`, `engine/merge.rs`, `event.rs`, `position.rs`,
  `render.rs`, and `summary.rs`.  They have good test coverage, and the review
  notes mark most of them as done.  They also encode lessons that were
  expensive to learn, such as scan budgets under selective filters and
  out-of-order detection.  `position.rs` is a partial exception.  The file
  stays, but the proposal above replaces two of its main types, `Cursor` and
  `LogStreamPosition`.
- Session persistence and `seeit`.  They may need changes once the tab and log
  stream model is settled, but they do not need a rewrite.

Rebuild these from a written spec:

- `bin/seer.rs` (11,500 lines).  The review notes describe tangled
  responsibilities, overlapping `LongOp` bookkeeping, and `Tab` fields that
  depend on its kind.  Its tests do not compile, so they offer little
  protection for keeping it.
- `StreamView` and `Viewport`.  Keep the design from `review/streamview.md`,
  but write new code once position semantics are defined.

The spec should settle these questions first:

1. What a position in a log stream means, and how it behaves when the filter
   or the set of sources changes.  The viewport, bookmarks, filter changes,
   and session restore all depend on this.
2. What a tab is responsible for and what a log stream is responsible for.
   This includes whether each has an id, a name, or both, and whether two tabs
   may share a log stream.
3. What ^C does during each kind of long operation, especially while the
   window is still filling.
4. How records that are not bunyan JSON (such as SMF lines) are placed in time
   within a merged stream.

## Not yet covered

The recommendation above does not mention everything in the repository.
This section lists the remaining modules and the issues that cut across
layers.

### Modules

- **`src/engine.rs`** (336 lines) is a thin wrapper that owns the sources and
  builds steppers.  It should stay.  It needs small changes for the position
  proposal.  `cursor_at_end()` becomes `StreamPosition::End`, and
  `stepper_batched` still has `XXX-dap` notes.
- **`src/stream.rs`** defines `LogStream`, which holds an id, a name, a filter,
  and display options.  It is small, but it sits in the open question about
  what a tab is responsible for and what a log stream is responsible for.  The
  spec should decide its shape.
- **The session modules** need different treatment from each other.
  - `src/session_store.rs` handles the on-disk directory and session lookup.
    It does not depend on the model, so it should stay.
  - `src/session.rs` defines the saved `Tab`, `Bookmark`, and `LogStream`
    data.  It will change with the spec and with `StreamPosition`.
  - `src/save_policy.rs` is the TUI's contract for deciding when to save.
    `review/dap-review.txt` questions whether its shape makes sense.  It
    should be revisited while the TUI is rebuilt.
- **`src/view_target.rs`** (907 lines) resolves `seeit --session`, `--tab`,
  and `--bookmark`, and it builds the command that `Y` prints.
  `review/dap-review.txt` suggests that `seeit` should not take a session id
  at all.  Instead, `Y` could print the equivalent filter, source, and
  display arguments.  That is a design decision for the spec, and the outcome
  decides whether most of this file survives.
- **`src/ui/`** is an empty stub with a `TODO-doc`.  An `XXX-dap` note says
  `Viewport` should move to `src/ui`, so this could become its home.
  Otherwise the module can be deleted.
- **`src/test_fixtures.rs` and `examples/gen_fixture.rs`** should stay.
  `review/dap-review.txt` notes that the example duplicates the fixture
  generators and that the fixtures might belong in a separate package.

### Tests, benchmarks, and the schema

- **`tests/session_lifecycle.rs` and `tests/seeit_session.rs`** build
  `Cursor` values and session types directly.  They will change along with
  the session format.
- **`tests/session_schema.rs`** compares the session schema against
  `tests/fixtures/session.schema.json`.  Any change to the model updates that
  file.  This is also where the migration question becomes concrete.
- **`benches/engine.rs`** has two benchmarks on `StreamView`.  They need to be
  rewritten against the new `Viewport` or dropped.  The other benchmarks can
  stay.
- **The `seer` unit tests that do not compile** go away with the TUI rebuild.
  Some of their scenarios, especially the seek and search cases, are worth
  carrying over as tests for the new code.

### Cross-cutting issues

- **The concurrency model.**  `CLAUDE.md` says to think carefully about
  concurrency.  Today, all work runs on the UI thread in small, time-limited
  steps.  Moving scans to a worker thread would change the design of
  `Viewport` a good deal.  For example, ^C could cancel a background task
  instead of dropping a cloned stepper.  The spec should choose a model
  before `Viewport` is rebuilt.
- **Features that reach into the layers marked "keep."**  SMF lines,
  CockroachDB logs, syslog, archives, and network sources all affect `Event`,
  `Source`, and how records without a time are ordered.  The spec should
  sketch these now, even if the work comes later.  Otherwise "keep" may turn
  into "rework" later.
- **Features that only affect the upper layers.**  These include named
  filters, filters that combine conditions with OR, undo and redo, plugins,
  and the "create filter" dialog.  They can wait, but the tab and log stream
  model should leave room for them.
- **Performance on real data.**  `CLAUDE.md` lists performance as the next
  step.  This assessment did not measure anything.  Its comments about speed,
  such as the cost of seeking by time with a linear scan, come from reading
  the code.
- **Session migration.**  This appears as an open question under the position
  proposal.  It also applies to any change in the tab and log stream model.
- **Project documents.**  The TODO lists in `CLAUDE.md` are partly out of
  date.  For example, the plan to consolidate `EventStream` onto `Stepper` is
  already done.  Once a direction is chosen, `CLAUDE.md` and the `review/`
  notes should be updated so that a new session does not follow old plans.
