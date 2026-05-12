# Seer

## Goal

A tool for human exploration of bunyan log files from Oxide support cases.  Two binaries from one library:

- **`seer`**: a `ratatui` TUI for interactive triage — scroll, filter, bookmark, merge across files, resume where you left off.
- **`seeit`**: a non-interactive development tool that prints filtered entries from the same files.  Not a throwaway proof-of-concept on the way to the TUI; a permanent artifact, useful for:
  - dev/debug iteration on the parser, filter language, and display formatting without ratatui in the way;
  - integration tests of the engine, driven by stdin/stdout;
  - scripting when you want grep-like output, not an interactive session;
  - reproducing a `seer` view non-interactively: `seeit --session ID --tab NAME` (or `--stream`/`--bookmark`) emits exactly what the TUI would draw.  Press `Y` in `seer` to get the matching command for the active view.

We're still in the exploratory phase.  There's no single milestone where the project is "done" or even "good enough".  There's some point where it's useful enough to share, but the features that define that point aren't yet known.  Hence: prioritize features, don't draw a line labeled "MVP".

Major goals (eventually, not all at once):

1. **Iteration on an investigation without losing your spot.**  Adding a filter, excluding a noisy message, switching files — all in-process.
2. **Resumable.**  TUI session state (filters, hidden fields, bookmarks, cursor) persists to disk so a crash or quit doesn't burn your work.
3. **Responsive.** Navigation and filter changes should be fast (a few seconds at most), even on real-world support data where a single file may take a minute or two to fully parse.

## Use cases

A user begins investigating a system problem.  They have a large set of potentially large files from a lot of different components.  Concretely, the Oxide system has 32 sleds, a handful of components that run on every sled, plus a handful of components that run on some subset of sleds.  Each component may have multiple _types_ of log files (e.g., the log from the main service _and_ the log from some sidecar-type service).  Each of these logs may have multiple files corresponding to different time ranges (log rotation).

Log files of interest, in decreasing order of priority:

- bunyan log format, with SMF log entries interspersed
- CockroachDB log format
- syslog format

The user may start with:

- the latest entries from a particular instance of a particular component
- a specific time range from a specific component
- something else

The user might want to do any of these things:

- explore the current log stream
  - scroll forward and backward, like vim (j, k, ^U, ^D)
  - scroll forward and backward in time (+15s, +30s, +1m, +5m, +10m, +1h)
  - search forward and backward
  - jump to a specific time
  - hide the current log entry
  - show only log entries like the current one
  - hide all log entries that look like the current one (details spelled out elsewhere; this would likely pop up a screen to add a new filter, prepopulated with "msg != <this log entry's message>").
  - show all log entries that look like the current one (similar)
  - apply a manual filter (including log-level filter)
  - show/hide various fields
  - enable/disable a named filter
- navigate to a different, specific log file
- navigate to a merged view of several log files (these could be the same type, as in "all three Nexus log files", or different types, like "all log files from sled X").  (Merging should generally be fast since we can assume log files are in-order.  Detect when that's not the case.)
- bookmark what they're currently looking at so they can come back to this exact view (set of log entries, including filters)
- navigate to a specific bookmark
- exit the program and resume exactly where they were later (same tabs open with the same streams)
- summarize the current log stream
  - count the number of entries
    - broken out by any number of fields
    - e.g., count by minute, count by "level", etc.
- save one or more filter rules together as a named filter

The actual UX design remains very TBD.  Feel free to suggest abstractions that would help facilitate the desired use cases.  Mention when proposed abstractions don't seem to make sense.

There's a bunch of data associated with a user's session:

* the set of files you're working on ("sources"),
* a set of named filters, which you can turn on and off individually
* the set of open tabs, each associated with a particular log stream.  A "log stream" could be viewed as a filter over all the data in all the sources.  All of one file's logs could be one log stream.  A merged view of all logs having some field could be another log stream.  Log streams have filters, bookmarks, and other configuration like which fields are being shown right now.  They exist independent of tabs.  They can be named (e.g., "Nexus logs").
* a set of bookmarks: each being a position in a log stream

Think of this a bit like Wireshark or a modular debugger: projects, teams, and individuals may want to customize the tool with their own functionality later.  TBD: how do we support this?  Some ideas for types of plugins:

- computed fields: given a structured log entry, add one or more fields to it (stateless?).
- create log streams: given a set of sources for a specific program or system, define some useful named log streams (e.g., "All Nexus logs" would merge logs from all "Nexus" components).
- analyzers: given a log stream, inject new events reflecting higher-level analysis.  e.g., given a sled agent log, inject an entry for "time is now synchronized", or given a Nexus log, inject an entry for "blueprint execution complete").  This could be used to look for specific patterns.
- detect component and apply default filters to any log streams created with it (e.g., hide authz entries by default)

In terms of TUI design:

- main pane should show one log stream
- a much smaller pane should summarize the log stream: filter information
- hideable panes:
  - show/configure what fields are present
  - various summary views of the current log stream
  - list of bookmarks, which can be named or unnamed
- many operations should pop up little dialogs
  - create a new log stream
  - create named bookmark should pop up a little dialog for naming it
  - show/hide log entries like this
  - tweak the current non-named filter
  - save current non-named filter as a new named filter rule

## Plan

There are multiple approaches here and I'm not yet sure how we'll want to proceed.

1. Derisk the concept: focus on the TUI, backed by synthetic data.  The UX drives the behaviors we need so that's a reason to start top-down.
2. Focus on the low-level pieces we know we'll need, like parsing JSON files.  Minimizes throwaway work, and it's easy to know how to proceed: we'll keep building things up until we get to what we're trying to build.
3. Focus on what we think are the key abstractions: projects, filters, log streams, etc, then work either up (toward TUI) or down (toward engine/storage).

We'll probably wind up doing a mix of these.  I'll give you more specific guidance about what I'm looking to do next.

We'll want to think carefully about the concurrency model to balance simple ownership (e.g., actors and message passing) with concurrency, backpressure, etc.

## Key feature ideas

These aren't necessarily in priority order or the order we'd implement them.

Basic functionality: display bunyan-format log files.  Plaintext that's not parseable as bunyan should either be attached to the previous bunyan log record or reported as its own kind of record in the stream.

**Filter rules:** be able to filter entries from the log.  Initially, these filters can be structured objects.  They don't need to boil down to a parseable text format.  e.g., `Filter::Include(FilterCriteria { criteria: vec![FilterField { "field": "name", "value": "Nexus" }]})`.  (Doesn't need to look exactly like this.)  (Alternately: maybe the rule's _runtime_ configuration is one of: include only, exclude, or ignore?)  There should be predicates based on `level`, `field=value`, regex on `msg`, regex on raw line, exact-message exclusion, etc.

**Named groups of filter rules.** Users should be able to take a collection of rules, give them a name, and toggle them on or off together.  (e.g., "exclude authz log entries").

**Persistent session state.**  All the information needed to reconstruct the user's session should be persisted to their home directory, under a configuration indexed by this "project".  This includes open tabs, existing log streams, etc.

**Persistence for named filter rules.** Named filters should be persisted separately from projects so that they can be used in other projects.

**Toggle which fields are displayed.**  This should be stored with the log stream configuration.

**Bookmarks with optional names.**  A bookmark is a position in a log stream so that a user can come back to whatever they were looking at.

**Undo/Redo**

**Support for CockroachDB log files.**

**Keyboard shortcuts** described above.

**Multiple tabs**, each looking at a specific log stream.

Plugins: see the ones mentioned above.

**Ability to define SMF events from Bunyan log files.**  This might imply the "analyzers" plugin.

**A way to list and name log streams.**

**Fields attached to log streams** (e.g., component "Nexus" or sled X).  These would function like fields on log entries but could be implemented more efficiently.

**Lazy loading.**  Real Oxide support log files can take 1-2 minutes each to fully parse; a dozen of them naively would mean 10-20 minutes of startup blocking.  That's unacceptable.  The loading process will have to be staged: first open all files, examine the first and last few log entries, and determine the time range and set of fields present.  It's not yet clear how much caching of the actual log content will be necessary.

Loading files regardless of how they're packed.  For example: you should be able to point it at a tarball or ZIP file of other log entries or even other zipped data.

Network sources (e.g., over ssh -- assume we have the same binary on the other side).

## Possible implementation pieces

- **Log format parsers**
- **Render module**: formatting a `LogEntry` to a display line, shared between CLI output and TUI rows.
- **Filter language** shared by CLI and TUI: level predicates, `field=value`, regex on `msg`, regex on raw line, exact-message exclusion.

## Code quality

**One package, multiple binaries, layered modules.**  No crate split yet, but modules are bounded the way the eventual crates would be — a clean layer line between `tui`, `engine`, `storage`, and the library exposes the engine to both binaries.  When extraction makes sense, it's a `cargo new -p` away, not a rewrite.

- All code should be clean per `cargo check --all-targets` and
  `cargo clippy --all-targets`.
- Favor strong typing as described by the associated RFD 643 skill.
- Strong bias toward all code being well-covered by unit tests and integration tests.  The CLI binary exists in part to make integration testing of the engine cheap (stdin/stdout fixtures).

## Module layout

One Cargo project, library + two binaries.  Module boundaries match
the eventual crate-extraction split.

Here's an example to give the flavor.  Don't view this as a target to go build.

```
src/
  lib.rs             // re-exports the public engine + storage API

  log_entry.rs       // shared types: LogEntry, Level, LineRef

  storage/
    mod.rs           // public API: open(path) -> Source; fetch entries
    source.rs        // Source { path, file, index, lru }
    scan.rs          // build LineRef index (stage A)
    parse.rs         // bunyan line parser (stage B)

  engine/
    mod.rs           // public API consumed by both binaries
    filter.rs        // Filter enum + evaluation; parser for filter args
    stream.rs        // merged iteration over multiple Sources, online
    render.rs        // formatting a LogEntry into a display line
                     // (used by both CLI output and TUI rows)
    view_state.rs    // cursor, visible fields, search, undo stack
                     //   — used by the TUI; not by the CLI
    session.rs       // serde load/save of session state, versioned
                     //   — used by the TUI; not by the CLI

  bin/
    seeit.rs         // one-shot: parse args, build filters, stream
                     // matching entries to stdout
    seer.rs          // ratatui app: event loop, render, input dispatch
```

Rules of the layer line:
- Binaries depend on the library; the library does not know about either binary.
- `engine` depends on `storage`.  `storage` does not know about purely user-level concerns (like projects or log entry rendering), but it could know about things like filters if it makes sense to push the logic down to this layer for performance.
- `engine::view_state` and `engine::session` are TUI-only members of the engine module — the CLI binary doesn't import them.
- Neither `engine` nor `storage` knows about ratatui.
- When these stop being true, that's the signal to extract crates.

All files should have the 3-line MPL-2.0 header.

Most Rust files should have a Rustdoc comment for the file.

## Risks called out by Claude

- **Render performance.**  ratatui re-renders every frame; only build line widgets for visible rows.  Compute the visible window from the cursor, do not materialize all lines.
- **Session-file rot.**  When changing the schema format for session configuration, ask if we want to do a migration.
- **Index correctness vs. speed.**  The substring/regex `time` extractor must agree with the full parser for the same line.  Add a property test: pick random parsed entries, re-extract via the index path, assert equal.

## Next steps

- Performance

## TODO list

### Summarizing fields in the view

When we process each log, keep track of distinct top-level JSON field names.  Keep only the top 10.

Let's add a new *kind* of tab, called a summary.  If the user hits `S` in the main view, a new tab is opened, starting with the filter dialog like usual.  But instead of displaying individual records, it displays summary stats.  For each of the top 10 fields (unioned across all sources), draw a histogram showing the top N distinct values, along with a histogram.  Here's inspiration, though I'd rather you put the most common values up top.

```
              key  ------------- Distribution ------------- count
           getpid |                                         1
        getrandom |                                         1
        getrlimit |                                         1
         lwp_exit |                                         1
           munmap |                                         1
        nanosleep |                                         1
            rexit |                                         1
         schedctl |                                         1
           sysi86 |                                         1
 lwp_cond_broadcast |                                         3
             mmap |                                         4
         readlink |                                         4
      resolvepath |                                         4
             stat |                                         4
        sigaction |                                         5
        sysconfig |                                         5
              brk |                                         9
             open |                                         10
       setcontext |                                         12
            gtime |                                         15
            pread |                                         39
             read |                                         84
            write |▏                                        96
         lwp_park |▏                                        152
          pollsys |▏                                        168
         p_online |▎                                        256
      lwp_sigmask |▋                                        505
            fcntl |█▉                                       1370
           openat |█▉                                       1370
            fstat |█▉                                       1378
            close |█▉                                       1379
         getdents |█▉                                       1382
            ioctl |█████▍                                   3836
          fstatat |██████████▍                              7310
            lstat |████████████▍                            8754
```

"Time" should be treated specially.  Take the whole time range represented by the log file and figure out which of these intervals would produce about 30 buckets: 1m, 1h, 1d.  Then create buckets with that interval size and show the count of records in each bucket (again, with a histogram).

Make sure there are comprehensive tests for the accuracy of this display.

### "Create filter" dialog

I want to make the "create filter" dialog easier to use.  I want folks to be presented with:

- choose sources:
  - default: merged view of all sources
  - or: select individual sources.  (note: there may be a lot of these.  Maybe
    this should involve another popup with checkboxes for each one, where you
    can scroll down with j/k and use spacebar to select/deselect)
- choose time range ("from" (optional) and "to" (optional))
- see available fields (known because we cache these with each file now) with the ability to create conditions for them

As folks edit these, the filter string should be rendered below.

It should still be possible to edit the filter string directly.  If it's parseable as something the dialog can understand, then you can go back and forth.  If not, then it should print a note about that and you won't be able to select the other fields.

New tabs should still open with this dialog.

### TODO

- parse SMF entries
- parse CockroachDB log
- need to teach Claude about slog_error_chain -- see SourceError
- confirmation dialog boxes need work
- `?` binding to pop up summary of key bindings
- second search (e.g., by time):
  - no feedback
  - super slow navigation after that (not as slow as re-parsing everything)
    - even showing all fields with F is super slow
  - setting a bookmark, closing the tap, and going to that bookmark is fast
- search history
- long-op coverage gaps left after the `G`/`g`/filter rebuild work:
  - `scroll_lines` at the window edge still uses unbounded `extend_*_batch`, so the first `k` after `G` on a selective filter can still freeze briefly.  Same long-op pattern as `LongOp::Seek` would address it.
  - `<` / `>` (`advance_time`) wasn't long-op'd; large time jumps under selective filters will freeze the UI.
  - `SeekFinalize::FrontOrBackFallback` runs `view.ensure_window` synchronously when the forward-from-cursor fetch came up empty.  Fine for typical bookmark navigation; a pathological filter with no matches anywhere could still freeze during finalize.
- add a marker in the logstream where there are bookmarks
- should bookmarks be navigable from any tab?
- press 'Y' over bookmark should open the `seeit` command dialog too
