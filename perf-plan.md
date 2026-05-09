# Performance Plan

## Goals

- Explore very large amounts of data without a comparable amount of memory.
- Initial loading should be as quick as possible.
- Navigation and re-filtering should be quick.  Pauses are acceptable when
  moving into regions of the dataset that have not yet been loaded and parsed,
  but only those regions — not everything.
- Stretch: after a full parse, exiting and reopening should reload the parsed
  state much faster than re-parsing.

## Approach

Two layers of work, taken together:

1. **Per-file metadata on open** for whole-file pruning.
2. **Byte-offset cursors** that let the engine fetch only the records the TUI
   needs, in either direction, from any position.

This avoids any "regex out the timestamp" path separate from the real parser.
Records are only parsed when actually requested.

### Storage layer: per-file metadata

When a source is opened, the storage layer reads the first and last records
(seek to start, read to first newline; seek to end, read back to the last
newline) and records:

- `earliest: Option<DateTime>`
- `latest: Option<DateTime>`
- `name: Option<String>` — the `name` field from the first entry, if present
- `hostname: Option<String>` — likewise

These are used for whole-file pruning during `query`:

- If a query specifies `name` or `hostname` and the file's value doesn't
  match, return zero records immediately.
- If a query specifies a time range disjoint from `[earliest, latest]`, return
  zero records immediately.

### Storage layer: byte-offset cursors and `query`

A cursor is a per-source byte offset.  A merged-stream cursor is a
`BTreeMap<SourceId, ByteOffset>`.

```rust
enum Direction { Forward, Backward }

fn query(
    &self,
    offset: ByteOffset,
    direction: Direction,
    count: usize,
    filter: &Filter,
) -> Vec<Record>;
```

**Convention:** in both directions, `offset` is the byte position at which the
next record would *start*.

- Forward: read from `offset` onward.
- Backward: read bytes preceding `offset`, locate the previous newline, parse
  the record between that newline and `offset`.  The cursor advances
  symmetrically either way.

**Backward reads** with a plain `File` use the usual pattern: seek to
`offset - chunk_size`, read a chunk, scan for the last newline, parse inward.
Worth encapsulating in a small read-backward-line iterator so it lives in one
place.

**Filter evaluation.**  Whole-file pruning by `name`/`hostname`/time-range
falls out of the metadata above.  For predicates that depend on the body
(`level=error`, `field=x`, regex on `msg`), the storage layer parses each
candidate as it scans.  Cost is bounded by records actually scanned, not the
whole file, as long as the filter is applied during the `query` call rather
than after it returns.

**Non-JSON lines.**  Bunyan files have SMF entries and stray plaintext mixed
in.  Existing policy is to attach them to the previous bunyan record or report
them as their own kind.  Whichever we choose, every record needs a
well-defined `(offset, length)`.  The simplest path is to fold attached
plaintext into the preceding record's byte range so it isn't separately
addressable.

**EOF/BOF.**  A source exhausted in the requested direction contributes no
records to the merge for that step.  A unit test confirming the merge keeps
moving when one source runs dry is worthwhile.

### Engine layer: merge with lookahead and lookbehind

The engine maintains per-source lookahead and lookbehind buffers and exposes a
forward/backward step API to the TUI.  Standard k-way merge by timestamp; when
a source's lookahead is exhausted in a direction, fetch another batch from it.

### TUI layer

Query only the visible window plus a small over-fetch buffer in each
direction.  Scrolling extends the buffer in the direction of motion; the
opposite-direction buffer is kept around to make small reversals free.

### Bookmarks

`(log_stream_id, BTreeMap<SourceId, ByteOffset>)`.  Trivially serializable,
stable across reopens, and captures merged streams naturally.

## Strong typing

Lean on the type system to make the offset/direction semantics hard to misuse:

- `ByteOffset(u64)` newtype rather than bare `u64`, so it can't be confused
  with counts or lengths.
- `RecordSpan { start: ByteOffset, len: u32 }` (or similar) rather than two
  bare integers, so length and offset can't be swapped.
- `enum Direction { Forward, Backward }` already in the sketch — keep it as
  the only way to ask for direction; no `bool reverse` overloads.
- Consider an enum-with-data for query results that distinguishes "matched
  the filter" from "scanned but didn't match" if that distinction ever needs
  to surface (e.g., for progress reporting).
- `Cursor` as a wrapper around `BTreeMap<SourceId, ByteOffset>` rather than
  exposing the map directly, so callers can't accidentally use it as a plain
  map.

The general principle from RFD 643 applies: any place where a runtime mistake
(wrong unit, wrong direction, swapped arguments) could be caught at compile
time, prefer the compile-time check.

## Testing

The storage layer is a very testable problem and deserves heavy coverage.
Strong bias toward unit tests directly against the storage API plus
integration tests through `seeit`.

Coverage targets:

- `query` symmetry: forward N records from `offset`, then backward N records
  from the position just past the last one returned, yields the original set
  reversed.
- Cursor round-trip: take a cursor, advance it, advance back, end up where
  you started.
- Boundary behavior: `offset = 0`, `offset = file_len`, single-record files,
  empty files, files ending without a trailing newline.
- Backward reads across chunk boundaries (records that straddle a chunk read
  boundary, records longer than a single chunk).
- Filter pruning: `name`/`hostname`/time-range mismatches return zero records
  without scanning, verified by either a counter on bytes read or a property
  test.
- Non-JSON lines: SMF and plaintext lines are handled per policy, and their
  presence doesn't desynchronize cursors.
- Merge across sources: per-source EOF in either direction doesn't stall the
  others; out-of-order timestamps within a single file are detected and
  reported.
- Property tests: random sequences of forward/backward steps from random
  starting positions arrive at consistent cursors and consistent record sets.

The `seeit` binary is well-suited to fixture-driven integration tests:
stdin/stdout fixtures of (input file, filter, cursor, direction, count) →
expected records.

## Suggested order of work

1. Per-file metadata on open (`earliest`, `latest`, `name`, `hostname`) and
   the file-level pruning hooks in `query`.
2. Storage `query(offset, direction, count, filter)`, including the
   backward-read helper.
3. Engine merge maintaining per-source lookahead and lookbehind buffers, with
   a forward/backward step API for the TUI.
4. Wire the TUI to query only its visible window plus a small over-fetch
   buffer in each direction.  The `Tab` gains a `StreamView` that owns the
   bounded window; scroll/`g`/`G` slide it.  Search, `<`/`>`, and bookmark
   navigation continue to scan the materialized cache as a transitional
   step — they work correctly within the window but don't extend lazily
   across its edges yet.
5. Replumb the App's `apply_search` / `step_search`, `advance_time`, and
   bookmark-navigation handlers through `StreamView::search_step`,
   `advance_time`, and `seek_to_cursor` so they walk the engine lazily and
   slide the window as needed.  Mechanical follow-up to step 4; the
   `StreamView` methods already exist.
6. Bookmarks as `(log_stream_id, cursor)`.  Replaces the
   `LogStreamPosition` (source, time, ordinal) anchor with a byte cursor.
   Both bookmark navigation (`seek_to_cursor`) and bookmark creation
   (`StreamView::cursor_before_record`, derived from the window's
   `front_cursor` plus the records preceding the selection) are now
   `O(1)` — no walk from byte 0 on either path.

## Deferred

Carried forward from steps 4–6; can land in any order, none block
each other.

- **Drop `materialize_streamview` from the navigation hot path.**  Today
  every search step / time step / bookmark seek that lands on a result
  rebuilds `Tab::events` / `formatted` / `event_for_line` /
  `first_line_for_event` from the streamview's window, cloning every
  cached `Event` and formatted line.  The render path already reads
  directly from `StreamView::rendered_lines`; the materialized vectors
  exist mainly to keep selection-mode, bookmark commit, and exclude
  filters working against the legacy `events: Vec<Option<EngineEvent>>`
  shape.  Switching those callers to read records via the streamview
  (by `RecordKey`) lets us drop the per-navigation rebuild.  Bounded but
  non-trivial — `WINDOW_SOFT_CAP` records of clones per `n`/`<`/`>`.
- **Single stepper construction in bookmark navigation.**
  `App::navigate_to_bookmark_cursor` builds an unfiltered stepper to
  read the bookmarked event for the filter check, *then* calls
  `StreamView::seek_to_cursor` which constructs another stepper at
  the same cursor under the active filter.  Either have
  `seek_to_cursor` return the first record it lands on so the filter
  check can read it from there, or push the filter check down into
  the streamview.
- **Retire `Engine::resolve_position` and `cursor_for_position`.**
  Step 6 dropped the binary's last consumer of `resolve_position`, and
  the deferred O(1) bookmark-creation work removed the binary's last
  consumer of `cursor_for_position` from the hot path (it remains as a
  fallback for test fixtures with synthesized events).  Both functions
  and their tests can come out once `LogStreamPosition` is no longer
  needed for selection-mode exclude filters either (today
  `materialize_streamview` still mints one per `Ok` event purely to
  feed the legacy shape `tab.events` expects — see the first deferred
  item).

Deferred indefinitely:

- **`mmap`-based access.**  Sticking with `File` for now.
- **Serialized parsed-record cache.**  Implementation cost is high
  relative to the savings once the cursor model is in place.
- **Sparse time index** (offset of every Kth record's timestamp, built
  during the first full pass) to make "jump to time T" O(log) instead
  of O(scan from start).  Add when time-jump feels slow on real data.
- **Cross-run caching of per-file metadata.**  The first-record /
  last-record probe should be cheap enough that this isn't pressing.

