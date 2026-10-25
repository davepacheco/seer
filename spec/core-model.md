# Seer core model

Status: first draft, 2026-10-25.  Nothing here is decided yet.

This document defines the concepts that the rest of seer is built on:
records and their order in time, positions, log streams, tabs, bookmarks, and
the concurrency model.  The `Viewport` spec and the TUI spec will build on it.

Each section describes the current behavior, proposes a model, and lists the
decisions that need an answer.  Each decision has an id (for example **D1**)
and a recommended answer.  The last section collects all of the decisions in
one list.

Background for many of these proposals is in
`review/foundation-assessment.md`.

## Vocabulary

These terms are used with the following meanings throughout this document and
the specs that build on it.

- **Source**: one log file (later, possibly an entry in an archive or a remote
  file).  A source is a sequence of bytes.  The source's format determines how
  those bytes divide into records.
- **Record**: one entry read from a source.  A record covers a contiguous range
  of bytes, and it may span several lines.  A record either parses into an
  event or is kept as text that could not be parsed.
- **Event**: the parsed contents of a record, such as a bunyan log entry.
- **Merged stream**: all records from all sources, in time order, with no
  filter applied.  This is a concept, not something seer materializes.
- **Log stream**: a saved definition that selects part of the merged stream
  and says how to display it.  It has a name.
- **Tab**: an open view onto a log stream.  It knows where the user is in that
  stream.
- **Position**: a point between two records in the merged stream.  A position
  does not depend on any filter.
- **Bookmark**: a saved, optionally named position.
- **Session**: everything needed to resume an investigation.  This includes
  the sources, log streams, tabs, and bookmarks.

## 1. Records and time order

### Current behavior

Each line of a source is one record.  The storage layer builds this
assumption in.  `scan_forward` reads one line at a time, and
`read_record_before` searches backward for the previous newline.  A line that
parses as bunyan JSON becomes an `Event`.  Any other line becomes a parse
error.  The merge picks
the record with the earliest time across sources.  It emits any parse error
immediately, ahead of every event, because a parse error has no time.  Ties
between sources go to the source that was added first.

### Proposed model

#### Record boundaries

A record is a contiguous range of bytes in its source.  The source's format
decides where each record begins and ends.  For example:

- **bunyan**: each JSON line is one record.  A line that is not JSON, such as
  an SMF line, is a separate record of its own.  Those lines come from a
  different writer than the JSON lines around them, so they are not part of
  the record before them.
- **CockroachDB**: an entry can span several lines, such as a long message or
  a stack trace.  All of those lines form one record.
- **syslog**: a record is usually one line.

The storage layer needs a format component that finds record boundaries.  It
only has to answer one question: does a record start at this line?  The
storage layer uses that answer in three places:

- **Reading forward**, it reads lines until the next line that starts a
  record.
- **Reading backward**, it walks back one line at a time until it reaches a
  line that starts a record.
- **After a jump** to an arbitrary byte offset, such as a step in a binary
  search, it moves forward to the next line that starts a record.

This approach requires that a format can recognize the start of a record by
looking at a single line, or at most a small, fixed amount of nearby text.
Bunyan JSON meets this requirement, and so do formats where each record
begins with a timestamp.  A format that can't meet it would need an index of
record starts, built by reading the whole file once.  That would undo much of
the benefit of lazy loading.

#### Oversized records

The storage layer limits how much of one record it keeps in memory.  Today it
reads each whole record into memory, which fails badly in two realistic
cases:

- **Very large records.**  Some components log large structured values, such
  as Nexus logging a whole blueprint.  Those lines can reach hundreds of
  kilobytes, so the limit has to be generous.
- **Damaged files.**  A file cut short by a crash can end in a long run of NUL
  bytes with no newline.  More generally, any long run of text with no record
  start would become one huge record.

The limit caps memory, not the record itself.  A record is still the
contiguous range of bytes up to the next record start.  When a record is
larger than the limit, the storage layer keeps only its first part in memory
(for example, the first 4 MiB).  It continues reading past that point without
storing the bytes, to find where the record ends.  Offsets and sort keys work
the same way for an oversized record as for any other record.

An oversized record is represented by its own error, separate from a parse
error.  Seer does not try to parse it, because a record cut off partway
through would almost never parse.  A separate error also lets the display
explain what happened, rather than reporting a misleading parse failure.  The
error carries the record's full length, and the record keeps its stored
prefix for display.  For example:

```rust
enum RecordError {
    /// the record could not be parsed in this source's format
    Parse(ParseError),
    /// the record is larger than the in-memory limit and was not parsed
    TooLarge { length: ByteLen },
}
```

An oversized record still needs a sort time.  It gets one in the same way as
any other record without a time of its own.

Scan budgets should count bytes as well as records.  Today a budget limits
only how many records a scan examines.  A single 50 MB record would count as
one record, and reading it could freeze the UI.

A single file can contain more than one kind of record.  Oxide service logs
mix bunyan JSON lines with SMF lines.  The format for such a file is a
combination that recognizes the start of either kind of record.

#### Sort keys

Every record has a **sort key**:

```rust
/// Sort key for a record in the merged stream.
struct RecordKey {
    /// time used to order this record; may be completed or inherited
    /// rather than taken from the record (see "How a record gets its sort
    /// time")
    sort_time: DateTime<Utc>,
    source_id: SourceId,
    offset: ByteOffset,
}
```

The field is called `sort_time` rather than `time` on purpose.  It is the time
seer uses to place the record, and it is not always a time the record
actually carries.  Code that needs a record's real timestamp must read it
from the record's event, where it may be partial or missing.

Keys are compared by sort time, then by source id, then by byte offset.  The
merged stream is all records sorted by key.  A filtered view is a subsequence
of the merged stream.

Ties between sources are broken by `SourceId`, not by the order in which
sources were added.  The order in which sources are added can change between
runs, but a source's id does not.

Comparing by source before offset has a useful effect.  All of one source's
records at the same time stay together in the merged stream, in file order.
Other sources' records at that time cannot fall between them.

#### How a record gets its sort time

A record's sort time comes from one of three places:

- **Exact**: the record carries a complete timestamp, as a bunyan record
  does.  The sort time is that timestamp.
- **Completed**: the record carries a partial timestamp, and seer fills in the
  missing parts.  SMF lines are the main example.
- **Inherited**: the record carries no timestamp at all, so seer borrows one
  from a neighboring record.  Panic output and other plain text are examples.

The record notes which of these applies, as a value like this:

```rust
/// how a record's sort time was obtained
enum SortTimeKind {
    Exact,
    Completed,
    Inherited,
}
```

The record keeps its timestamp exactly as written, or no timestamp if it had
none.  For an SMF line, that is the partial timestamp.  The completed or
inherited time exists only in the sort key.  Today's `Event` requires a full
time, so the more general record type will need to allow a partial timestamp
or none.  Keeping the two apart means that code can always tell a real time
from one seer supplied.  The display relies on that distinction (see
section 8).

#### Completing partial timestamps

SMF lines carry a timestamp in a form like `[ Oct 24 17:45:01 ... ]`.  That
gives the month, day, and time of day, but no year and no time zone.  Some
sources contain only SMF lines, so this case is common.  For that reason, SMF
timestamp parsing is part of the first phase of work.

- **Time zone.**  Oxide sleds log in UTC, so seer assumes UTC.
- **Year.**  Seer fills in the year from a reference date for the source.
  These are the possible references, from most to least preferred:
  1. a date derived from the file's name, using a heuristic to be specified
     later;
  2. the file's modification time, which may not be accurate after files are
     copied or unpacked;
  3. the overall time range of the other sources in the session;
  4. a year entered by the user.

Whatever the reference, the year must be computable from the record and the
reference date alone, without reading the rest of the file.  Otherwise,
binary search by time would not work on these sources.  This is possible as
long as a file covers less than a year.  For example, when the reference is
the file's modification time, the record's year is the modification year if
the record's month and day come on or before the modification date.
Otherwise, it is the year before.

#### Records with no timestamp

A record with no timestamp at all gets the sort time of the nearest record
**before** it in the same source that has an exact or completed time.
Because it has a larger offset than that record, it sorts directly after it.

Records at the very start of a file, before any record with a time, get
their sort time from the first record **after** them that has one.  A file
with no timestamps of any kind is covered by **D3**.

The inheritance rule adds two requirements to the storage layer.  Reading
backward, it has to keep reading until it finds a record with a time to take
the time from.  When a search by time lands on a record without a time, it has to move
forward to the next record that has one.

#### Sources that are out of order

The model assumes that each source is sorted by time.  If a source is not
sorted, the merge still emits that source's records in file order, so the
output is not sorted by key.  Searching by time in that source can also give
the wrong answer.

The engine should detect this while reading, using the `OutOfOrder` warning
that `SourceError` already defines but never emits.  After that, it should
mark the source as unsorted and show the warning.  For an unsorted source,
looking up a position falls back to a linear scan.

### Decisions

- **D1.** How are record boundaries found?  *Recommendation:* Each format
  supplies a check that recognizes the start of a record from a single line,
  as described under "Record boundaries."  The format also decides whether a
  line it cannot parse belongs to the record before it or stands alone.  In
  bunyan logs, such a line stands alone.  In CockroachDB logs, a continuation
  line belongs to its entry.  Before relying on this design, we should check
  that CockroachDB's log format lets a record start be recognized from one
  line.
- **D16.** How are oversized records handled?  *Recommendation:* As described
  under "Oversized records."  Keep only a prefix in memory, represent the
  record with its own `TooLarge` error rather than a parse error, and count
  bytes in scan budgets.  Two details remain open.  The default limit should
  be chosen by looking at how large legitimate records get in real Oxide
  logs.  We also need to decide whether the user can raise the limit for a
  session, to see the whole of an oversized record.
- **D2.** Should ties between sources be broken by source id?
  *Recommendation:* Yes.
- **D3.** Where does a source with no timestamps of any kind go?  This
  excludes sources with only SMF timestamps, which **D17** covers.
  *Recommendation:* Treat its records as having the earliest possible time,
  so they appear at the start of the merged stream, and show a warning that
  the source has no timestamps.  This case should be rare.
- **D17.** How are partial timestamps completed?  *Recommendation:* As
  described under "Completing partial timestamps."  Assume UTC.  Fill in the
  year from a reference date, preferring a date derived from the file name.
  The file name heuristic will be specified later.

## 2. Positions

### Current behavior

A position is a `Cursor`, which stores one byte offset per source.  Those
offsets are only meaningful under the filter that produced them.  The details
are in `review/foundation-assessment.md`.

### Proposed model

A position is a **cut** in the merged stream.  It splits every record into
"before" and "after" by comparing the record's key against a bound:

```rust
enum StreamPosition {
    /// before every record
    Start,
    /// before every record whose key is at least this value
    Before(KeyBound),
    /// after every record
    End,
}

/// A value in key order.  It need not be the key of an existing record.
struct KeyBound {
    sort_time: DateTime<Utc>,
    source_id: SourceId,
    offset: ByteOffset,
}
```

A `KeyBound` does not have to name a real record.  That makes several
operations simple:

- The position just before record R is `Before(key(R))`.
- The position just after record R is `Before` a bound that equals R's key
  except that its offset is the end of R.  That bound is greater than R's key
  and less than the key of anything that follows R.
- The position at time T is `Before(T, lowest source id, offset 0)`.  This is
  how jumping to a time and stepping by time would work.

Positions have these properties:

- **They don't depend on the filter.**  The same position splits every filtered
  view at the same point.
- **They survive changes to the set of sources.**  Adding or removing a source
  does not change what a position means for the other sources.
- **They survive a changed file, approximately.**  If a file changes since a
  session was saved, its offsets may no longer match its records.  The time in
  the bound still places the position correctly with respect to every other
  source.

### How the stepper uses a position

`Engine::stepper` takes a `StreamPosition` instead of a `Cursor`.  For each
source, it finds the offset of the first record whose key is at least the
bound.  Because sources are sorted, it can use a binary search by time and
then a short scan forward to settle ties.  After that, the stepper works as it
does today.  Its per-source offsets are a private cache that is only valid
under its filter.

`Stepper::cursor()` goes away.  Callers build positions from the records the
stepper returns, using `before(&record)` and `after(&record)`.  This makes it
impossible to save the stepper's internal offsets by mistake.

Changing the filter means building a new stepper from the current position.
`Stepper::set_filter` goes away.

### Positions and the anchor

The `Viewport` anchors on a record and a line within that record.  It should
also remember the position just before the anchor record.  When the filter
changes, the `Viewport` uses that position to find its place in the new
filtered view.

The anchor record might be hidden by the new filter.  In that case, the view
shows the first visible record after the position.  If no visible record
follows, it shows the last visible record before the position.  The
`Viewport` keeps the original position until the user moves.  So if the user
changes the filter and then changes it back, the view returns to the exact
record it started on.

### Decisions

- **D4.** Should positions be cuts defined by a key bound, as described
  above?  *Recommendation:* Yes.
- **D5.** What should the view show when a filter change hides the anchor
  record?  *Recommendation:* Show the next visible record, or the previous one
  when nothing follows.  Keep the original position until the user moves.
  Show a short message saying that the anchor record is hidden.
- **D6.** What should the progress percentage in the status line measure?
  Today it adds up the cursor's byte offsets.  *Recommendation:* Use the
  anchor's time, measured against the time range of the sources in the
  stream.  It is cheap to compute, and it is more meaningful than bytes
  across files of very different sizes.

## 3. Log streams and tabs

### Current behavior

- `LogStream` has an id, a name, a filter, and display options.  The filter
  includes `source_id=~` conditions that choose which sources take part.
- `Tab` has a name, the id of a stream, a kind (`Stream` or `Summary`), and a
  saved `Cursor`.  Its doc comment says that two tabs may target the same
  stream.
- Bookmarks are stored per stream.
- The Bookmarks tab exists when there is at least one bookmark.  It is not
  stored in the session.
- The last search and the step size for `<` and `>` belong to the whole app,
  not to a tab.

The review notes in `review/dap-review.txt` ask what each concept is
responsible for, whether each needs an id or a name, and whether two tabs
should ever share a stream.

### Proposed model

A **log stream** answers "which records, shown how?"  It is a saved
definition.  It exists whether or not a tab shows it, and closing a tab does
not lose it.  It has four parts:

- **sources**: which sources take part.  This would be a separate field,
  rather than `source_id=~` conditions inside the filter.
- **time range**: an optional start time and end time.  This would also be a
  separate field.  The engine can turn it into starting and ending positions
  with a binary search, instead of checking every record.
- **filter**: conditions on individual records.  After this change, a filter
  only ever applies to records.
- **display options**: which fields to show and how to render them.

These four parts match the "create filter" dialog described in `CLAUDE.md`,
which offers sources, a time range, and conditions on fields.

A **tab** answers "where am I, and what am I looking at?"  It shows exactly
one log stream.  Its kind determines what else it holds:

```rust
enum Tab {
    /// records from one log stream
    Records { stream: LogStreamId, anchor: Anchor },
    /// a histogram of one log stream's records
    Summary { stream: LogStreamId, spec: SummarySpec },
    /// the list of bookmarks
    Bookmarks,
}
```

This replaces today's `Tab` with a `kind` field, which the review notes and
the TODO list both flag.

### Where each piece of state lives

| State                              | Lives on                     |
| ---------------------------------- | ---------------------------- |
| sources, time range, filter        | log stream                   |
| display options                    | log stream                   |
| name shown in the tab strip        | log stream (tabs don't have their own) |
| anchor (position and line)         | tab                          |
| last search and its direction      | tab                          |
| step size for `<` and `>`          | tab                          |
| search history                     | session                      |
| bookmarks                          | session, each linked to a log stream |

### Decisions

- **D7.** Should two record tabs be allowed to show the same log stream?
  *Recommendation:* No.  If they shared a stream, changing the filter in one
  tab would change the other tab too, which would be surprising.  Opening a
  stream that already has a record tab switches to that tab.  To look at two
  places in the same data, the user duplicates the stream, which creates a
  new stream with the same definition.
- **D8.** Is a summary tab a live view of its stream, or a snapshot?
  *Recommendation:* Make it a snapshot.  It copies the stream's definition
  when it is created, and it doesn't change when the stream's filter changes.
  A summary can take a long time to compute.  Recomputing it every time the
  filter changes would be slow, and it could also be confusing.  The tab
  should show the definition it was computed from.  This also means a summary
  tab does not count against **D7**.
- **D9.** Should tabs have names of their own?  *Recommendation:* No.  A
  record tab shows its stream's name.  A summary tab shows a name derived from
  its stream.  Without separate names, renaming a stream renames its tab,
  which is what users would expect.  `--tab` in `seeit` would then match
  stream names.
- **D10.** Should the set of sources and the time range be separate fields of
  a log stream, rather than conditions inside the filter?
  *Recommendation:* Yes.  This answers the question in the review notes about
  separating source conditions from other conditions.  It also lets the
  engine fix its set of sources and its time bounds when it builds a stepper.
- **D11.** Should the last search and the step size for `<` and `>` move to
  the tab?  *Recommendation:* Yes, and they should be saved with the tab.

## 4. Bookmarks

### Current behavior

A bookmark stores a `Cursor`, the id of a stream, and cached display fields
(source, time, name, and the start of the message).  Jumping to a bookmark
uses the stream's current filter.  Creating a bookmark currently panics,
because `Viewport::cursor_before_record()` is not implemented.

### Proposed model

A bookmark stores:

- an id and an optional name;
- a position, which is `Before(key)` of the bookmarked record;
- the id of the log stream it was created in;
- the cached display fields, as today.

A bookmark does not need to store a filter, because its position does not
depend on one.  Jumping to a bookmark opens its stream's tab and anchors at
its position.  If the stream's current filter hides the bookmarked record,
the rule from **D5** applies.

Because a position doesn't depend on the stream either, a bookmark can be
opened in any stream that includes its source.  This answers the TODO item
about making bookmarks reachable from any tab.

### Decisions

- **D12.** Can a bookmark be opened in a stream other than the one it was
  created in?  *Recommendation:* Yes, when that stream includes the
  bookmark's source.  The bookmark's own stream stays the default.
- **D13.** Should a bookmark record the filter that was active when it was
  created?  *Recommendation:* No.  It isn't needed for correctness, and it
  would raise a new question about which filter to use when jumping to it.

## 5. Concurrency

### Current behavior

All work runs on the UI thread.  Every slow operation is broken into small
steps.  The TUI runs steps in a loop until a time limit passes, then redraws
and checks for input.  A seek works on a clone of the stepper, so pressing ^C
drops the clone.

This approach works, but it has costs:

- Every slow operation has to be written as a resumable state machine.
  `SeekOperation`, the priming step for backward search
  (`needs_step_forward`), and the old `LongOp` code all come from this
  requirement.
- A single slow read, such as a very long line or a slow disk, stalls the UI.
  The step size limits how many records are scanned, not how long a scan
  takes.
- Work cannot run in parallel across sources.  Parsing is mostly CPU time.
  With dozens of large files, operations like a summary or a search could run
  several times faster across cores.

### Options

**A. Keep the current model.**  This is simple and deterministic, and tests
are easy to write.  The costs listed above remain.

**B. Run slow operations as jobs on worker threads.**  The UI thread owns all
view state.  A seek, a search, a summary, or filling a window becomes a job.
Each job owns its own stepper, which is cheap to clone.  The job writes
progress to shared counters and sends one result message when it finishes.

- Every job carries a generation number.  The UI ignores results whose
  generation is out of date.
- ^C sets the job's cancel flag and moves to a new generation.
- Jobs are written as ordinary loops that check the cancel flag, not as
  state machines.
- Work inside a job can be split across sources to use several cores.
- Moving within the part of the window that is already filled stays
  synchronous on the UI thread, so ordinary scrolling has no added delay.

The costs of option B are some shared state and harder tests.  Tests would
need a way to wait for a job to finish without polling.  `Source` is already
`Send + Sync`, and `Stepper` holds only `Arc` references, so the engine is
close to ready for this.

`seeit` stays synchronous under either option.  It runs each operation as a
plain loop to completion.

### Decisions

- **D14.** Which concurrency model should the rebuilt `Viewport` and TUI use?
  *Recommendation:* Option B.  The `Viewport` design already treats a seek as
  separate, speculative work that can be thrown away.  Option B expresses that
  directly, and it removes most of the complexity in today's step-based code.
  This is the decision with the largest effect on the `Viewport` spec, so it
  should be settled first.

## 6. The session file

### Current behavior

The session file is version 1.  A comment on `Session` says that there are no
saved session files that need to stay readable, so fields do not use
`#[serde(default)]`.

### Proposed changes

The decisions above change the session format in several ways:

- `Cursor` becomes `StreamPosition` in tabs and bookmarks.
- `LogStream` gains separate fields for sources and a time range.
- `Tab` becomes an enum.  It loses its own name and gains the last search and
  the time step.

### Decisions

- **D15.** Do existing session files need to keep working?
  *Recommendation:* If the comment on `Session` is still accurate, no.
  Increase the version to 2 and refuse version 1 files with a clear message.
  Otherwise, convert them.  Each `Cursor` would be converted by building a
  stepper under the stream's filter and taking the position before the next
  record.

## 7. Deferred features

These features are out of scope for now.  The model above should leave room
for them.

- **Named filters** that can be turned on and off.  A log stream could refer
  to named filter groups in addition to its own conditions.
- **Filters with OR**, through a tree of conditions in place of today's flat
  list.  This change affects only `filter.rs`.
- **CockroachDB logs and syslog.**  These need a more general record type in
  place of today's bunyan-shaped `Event`.  They also need their own rules for
  record boundaries, as described in section 1.  The sort key does not
  change.
- **Archives and remote sources.**  These would be new `Source`
  implementations.  An archive member can be extracted to a cache directory so
  that it supports random access.
- **Undo and redo.**  Log stream definitions and tab anchors are small values,
  so an undo stack could store snapshots of them.
- **Plugins.**  Computed fields and analyzers that add events would need the
  most care.  An added event needs a sort key, and the model doesn't yet say
  how a synthetic record gets one.

## 8. Notes for later specs

These requirements follow from this model, but they belong in specs that
don't exist yet.  They are recorded here so they are not lost.

- **Mark records whose time is not exact.**  The TUI should show which
  records have a completed or inherited sort time, as described under "How a
  record gets its sort time."  One way is a different background color, with
  separate treatments for completed and inherited times.  `seeit` could use a marker in its
  output for the same purpose.  This belongs in the TUI spec and the
  `Viewport` spec.
- **Mark oversized records.**  The display should show the stored prefix of
  an oversized record along with a note that gives its full size, such as
  "record truncated, 52 MB."  It should not present the record as a parse
  failure.

## Summary of decisions

| Id  | Question                                              | Recommendation |
| --- | ----------------------------------------------------- | -------------- |
| D1  | How are record boundaries found?                      | Each format recognizes a record start from one line. |
| D2  | Break ties between sources by source id?              | Yes. |
| D3  | Where does a source with no timestamps at all go?     | At the start, with a warning. |
| D4  | Positions as cuts defined by a key bound?             | Yes. |
| D5  | What to show when the anchor record is hidden?        | Next visible record, else previous.  Keep the original position until the user moves. |
| D6  | What does the progress percentage measure?            | Time. |
| D7  | Two record tabs on one log stream?                    | No. |
| D8  | Is a summary tab live or a snapshot?                  | Snapshot. |
| D9  | Do tabs have their own names?                         | No. |
| D10 | Separate sources and time range from the filter?      | Yes. |
| D11 | Move the last search and time step to the tab?        | Yes. |
| D12 | Open a bookmark in another stream?                    | Yes, if that stream includes its source. |
| D13 | Store the filter with a bookmark?                     | No. |
| D14 | Concurrency model?                                    | Jobs on worker threads (option B). |
| D15 | Keep old session files working?                       | No, if the comment on `Session` is still accurate. |
| D16 | How are oversized records handled?                    | Keep a prefix, use a separate `TooLarge` error, count bytes in budgets. |
| D17 | How are partial timestamps completed?                 | UTC; year from a reference date, preferring one from the file name. |
