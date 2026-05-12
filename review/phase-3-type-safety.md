# Phase 3 — Type safety (RFD 643)

Findings classified per the type-safety-review skill's anti-pattern
categories.  "Blocking" means likely to cause silent bugs or data
loss; "Suggestion" means robustness improvement.

`````
## Type Safety Review

### Mode
Targeted review of the seer library and both binaries.

### Findings

#### [BLOCKING] Missing full-struct destructuring — src/stream.rs:195, :210
**Problem:** `LogStream::render_opts` and `LogStream::set_render_opts` copy six fields between `RenderOpts` and `LogStream` by name, without destructuring. Adding a new field to `RenderOpts` will silently fail to round-trip through these methods — the new dimension would be ignored when saving to the session and reset to its default when loading. The doc comment at :207 calls this out as a design choice for schema evolution, but the schema-evolution argument applies to the persisted shape, not to the copy methods themselves.
**Fix:** Destructure on both directions so the compiler flags additions:
```rust
pub fn set_render_opts(&mut self, opts: RenderOpts) {
    let RenderOpts {
        show_extras, show_date, hostname, show_pid, show_name, show_raw,
    } = opts;
    self.show_extras = show_extras;
    self.show_date = show_date;
    self.hostname_display = hostname;
    self.show_pid = show_pid;
    self.show_name = show_name;
    self.show_raw = show_raw;
}
```

#### [BLOCKING] Multiple representations — src/render.rs:134
**Problem:** `RenderOpts.show_raw` invalidates every other field on the struct. The doc comment (:140-145) admits as much: "Bypasses every other field in this struct: raw mode shows the line as it appears on disk and ignores the column toggles." So a value like `RenderOpts { show_raw: true, show_extras: true, show_date: false, … }` is representable but the contradictory pieces are silently ignored. Two consumers reading `show_extras` could disagree about whether to honor it under raw mode.
**Fix:** Replace the flat struct with an enum that makes the cases mutually exclusive:
```rust
pub enum RenderMode {
    Raw,
    Formatted {
        show_extras: bool,
        show_date: bool,
        hostname: HostnameDisplay,
        show_pid: bool,
        show_name: bool,
    },
}
```
`format_event` already branches on `show_raw` at the top, so call sites collapse cleanly. The session struct on `LogStream` will need a matching reshape (see also Finding 1).

#### [BLOCKING] Multiple representations — src/bin/seer.rs:1396 (`Tab`)
**Problem:** `Tab` carries both `kind: TabKind` and `streamview: Option<StreamView>`, with the invariant (in the doc at :1406-1413) that `streamview.is_some()` iff `kind == TabKind::Stream`. Two fields encoding the same state. Other parts of the code carry their own duplicated invariants: `Tab.events` / `Tab.formatted` are non-empty for Stream tabs and empty for Summary tabs.
**Fix:** Lift the union into the kind itself, e.g.:
```rust
enum TabContent {
    Stream { view: StreamView, rendered: RenderedRows },
    Summary { rendered: RenderedRows },
}
```
The Bookmarks pane is already a separate synthetic-tab path, so the enum doesn't need a `Bookmarks` variant.

#### [BLOCKING] `Vec` when uniqueness matters — src/session.rs:199
**Problem:** `Session.sources: Vec<SessionSource>` is a list of objects each carrying an `id: SourceId`. The documented invariant is that source ids are unique within a session, but the type does not enforce it. A future code path that pushes a duplicate would compile and only fail downstream.
**Fix:** Use `iddqd::IdOrdMap<SessionSource>` — the codebase already uses it for `Session.streams`. Implement `IdOrdItem` on `SessionSource` keyed by `id`. The `serde` form stays a JSON array, so no on-disk migration is required.

#### [BLOCKING] `Vec` when uniqueness matters — src/session.rs:216
**Problem:** `Session.user_bookmarks: BTreeMap<LogStreamId, Vec<Bookmark>>` stores per-stream bookmark lists. Each `Bookmark` has its own `id: BookmarkId`; the inner `Vec` has no uniqueness guarantee. `Session::remove_bookmark` already iterates looking for an id match (:255), and the linear scan is part of why the type is wrong: a `Vec` forces O(n) lookup where the keyed shape would be O(log n).
**Fix:** `BTreeMap<LogStreamId, IdOrdMap<Bookmark>>`, with `Bookmark` impl'ing `IdOrdItem`. `add_bookmark` becomes an `insert_unique` (it can no longer silently double-insert); `remove_bookmark` becomes a direct lookup.

#### [BLOCKING] Multiple representations — src/bin/seer.rs:669 (`RenderedRows`)
**Problem:** `RenderedRows.events: Vec<Option<EngineEvent>>` uses `None` to signal "this row is a parse error placeholder". The matching `formatted` line carries the error message string; nothing prevents the two from disagreeing. Three parallel vectors (`events`, `formatted`, `event_for_line`, `first_line_for_event`) all encode pieces of the same record-line mapping with raw `usize` indices.
**Fix:** Replace `Vec<Option<EngineEvent>>` with `Vec<Row>` where:
```rust
enum Row { Event(EngineEvent), Error(String) }
```
For the index vectors, see also the "Missing newtypes" finding below — type-tagging `usize` as `LineIdx` and `EventIdx` would prevent reversed-mapping bugs.

#### [SUGGESTION] Weak enum / bool usage — src/filter.rs:99
**Problem:** Four `Predicate` variants (`LevelEquals`, `FieldEquals`, `MsgMatches`, `SourceIdMatches`) carry a `negated: bool` field. The matching uses XOR (`field_matches(…) ^ *negated` at :181) which works but obscures intent. The DSL parser writes `/* negated = */ false` and `/* negated = */ true` at every call site (:290, :294, :309, :312, :323) — a clear signal the boolean reads poorly without a label.
**Fix:** Introduce `enum Polarity { Affirm, Deny }`. Match arms become `match polarity { Polarity::Affirm => result, Polarity::Deny => !result }`, which is easier to read and impossible to invert by accident.

#### [SUGGESTION] Magic literals — src/engine/merge.rs:48, src/streamview.rs:39
**Problem:** Both files define `const BATCH_SIZE: usize = 64;` with the same value, and the streamview comment ("Matches the storage layer's batch size so we don't over-fetch") asks the reader to keep them in sync. The compiler cannot enforce that.
**Fix:** Define once (e.g. `pub const FETCH_BATCH_SIZE: usize = 64;` in `engine` or at the lib root) and import in both places.

#### [SUGGESTION] Magic literals — src/event.rs:152, :196
**Problem:** The bunyan-level numeric mapping `10 ↔ Trace, 20 ↔ Debug, …, 60 ↔ Fatal` is spelled out twice — once in `Level::as_bunyan_number` and again in `TryFrom<u8> for Level`. The `JsonSchema` impl at :243-250 also lists the literals a third time. A future variant requires updating all three.
**Fix:** Either move the mapping into a single `const TABLE: &[(u8, Level)]` walked by both directions, or generate the impls with a small macro.

#### [SUGGESTION] Missing newtypes — src/source.rs:134, src/engine/merge.rs:132
**Problem:** Byte counts appear pervasively as bare `u64`: `QueryRecord.length`, `MergeRecord.length`, `QueryBatch.walked_bytes`, `ParseStats.bytes`, `bytes_read`, the various `total_bytes` fields on `LongOp` variants. The codebase already has `ByteOffset(u64)` for positions but no symmetric type for lengths. A function accepting both an offset and a length takes two `u64`s and any caller can transpose them.
**Fix:** Define `ByteLen(u64)` with the same affordances as `ByteOffset`. Arithmetic between them stays compiler-checked (`ByteOffset + ByteLen → ByteOffset` is the only sensible combination).

#### [SUGGESTION] Missing newtypes — src/bin/seer.rs:671, :672, :1014, :1097, :1180
**Problem:** Index fields use raw `usize`:
- `RenderedRows.event_for_line: Vec<usize>` (indices into `events`)
- `RenderedRows.first_line_for_event: Vec<usize>` (indices into `formatted`)
- `tab_idx: usize` on every `LongOp` variant (index into `App.tabs`)
- `Selection.event_idx: usize` (index into `Tab.events`)
- `Tab.viewport_top: usize` (line index)

A single `usize` parameter is easy to swap; the `event_for_line` / `first_line_for_event` pair is particularly accident-prone since they map *between* indices of two different domains.
**Fix:** Introduce three newtypes: `TabIdx(usize)`, `EventIdx(usize)`, `LineIdx(usize)`. Conversion lookups become typed: `event_for_line.get(LineIdx) -> Option<EventIdx>`. (No conversion impl between them — that's the whole point.)

#### [SUGGESTION] Weak enum / bool usage — src/streamview.rs:256-257
**Problem:** `forward_eof: bool, backward_eof: bool` on `StreamView` are paired booleans keyed by direction. The codebase already has `Direction { Forward, Backward }` (`src/source.rs:105`). A pair-of-bools is the classic "should be an `EnumMap<Direction, bool>`" shape.
**Fix:**
```rust
struct DirectionalEof { forward: bool, backward: bool }
impl DirectionalEof {
    fn get(&self, dir: Direction) -> bool { … }
    fn set(&mut self, dir: Direction, value: bool) { … }
}
```
Or, since `Direction` is small and `Copy`, an `enum_map::EnumMap<Direction, bool>` if the crate is acceptable.

#### [SUGGESTION] Weak enum / bool usage — function parameters
**Problem:** Several functions take a `bool` whose meaning is opaque at the call site:
- `render::format_time(time, show_date: bool)` (`src/render.rs:112`)
- `Tab::advance_time(forward: bool)` (`src/bin/seer.rs:3390`)
- `Tab::time_anchor_idx(prefer_forward: bool)` (`src/bin/seer.rs:1791`)
- `Tab::jump_to_match(direction, exclusive: bool)` (`src/bin/seer.rs:3279`)
- `SourceWindow::set_eof(dir, value: bool)` (`src/engine/merge.rs:251`)

Each call passes a literal `true` or `false` whose meaning the reader has to look up in the signature.
**Fix:** For `forward`, reuse the existing `Direction` enum. For `show_date`, introduce a `DateInclusion` enum or fold the param into `RenderOpts` (the function is internal). For `exclusive`, `enum Anchor { IncludeCurrent, ExcludeCurrent }`. The pattern is the same throughout: every bool parameter that survives a code review is one a future caller can pass backwards.

#### [SUGGESTION] Multiple representations — src/engine/merge.rs:70 (`Cursor`)
**Problem:** A `Cursor`'s `BTreeMap<SourceId, ByteOffset>` treats "source absent from the map" as equivalent to "source mapped to `ByteOffset::ZERO`". The doc comment makes this explicit at :62. Useful for `Cursor::default()` walking from the start of every source — but it means the cursor doesn't distinguish "haven't recorded this source's position" from "explicitly recorded position 0". For pure navigation that's fine; for serialization round-trips and diff'ing two cursors, the silent equivalence will eventually bite.
**Fix:** Either accept the convention and document it next to `PartialEq` (so a future change to "compare maps structurally" is conscious), or have `Cursor` normalize on construction by populating every known source with at least `ByteOffset::ZERO`. The choice depends on whether absent-vs-zero is observable elsewhere; today it appears not to be.

#### [SUGGESTION] Stringly-typed values — src/filter.rs:115, :220
**Problem:** `Predicate::FieldEquals { name: String, value: String }` and the helper `field_matches(event, name, &str, value: &str)` route `name` through a hand-written switch over `"name"`, `"hostname"`, `"pid"`, `"msg"`, `"v"`, falling through to `extra`. This is partly intentional — the user is allowed to filter on any field they see — but the bunyan *core* fields could be a typed sub-case, with the string variant reserved for `extra`:
```rust
enum FieldName {
    Core(CoreField),         // Name, Hostname, Pid, Msg, V
    Extra(String),
}
```
The DSL parser would convert `"name"` to `FieldName::Core(CoreField::Name)`, etc. The benefit is that consumers like `SourceMetadata::excludes_all` (`src/source.rs:330`) would match on a typed `CoreField` instead of a string. Not severe — the current string code is well-localized.

#### [SUGGESTION] Implicit runtime panics — src/bin/seer.rs:2138, :2220, :3169
**Problem:** A handful of bare `.unwrap()` calls in production paths without an explanatory comment:
- `self.store.as_ref().unwrap()` (:2138)
- `self.session.streams.get(&stream_id).unwrap()` (:2220)
- `self.session.streams.get(&target_stream).unwrap().filter.clone()` (:3169)

Many other call sites use `.expect("…")` with a clear reason ("stream exists", "freshly-minted LogStreamId is unique", etc.). The bare unwraps are stylistic outliers; standardizing them as `.expect()` with a short justification would match the rest of the file.
**Fix:** Replace each bare `.unwrap()` with `.expect("…")` carrying the invariant that makes the panic impossible — same shape as the surrounding code.

#### [SUGGESTION] Implicit runtime panics — src/streamview.rs (`as usize`/`as isize` casts)
**Problem:** Navigation arithmetic in `scroll_lines` (`:1108-1178`), search (`:1556-1616`), and `clamp_anchor` (`:1650`) uses `as usize` / `as isize` between signed-delta and unsigned-index. On 64-bit platforms there is no truncation; on 32-bit, a deque larger than `i32::MAX` would underflow silently. The line `(line as isize + remaining) as usize` (:1148) is a sign-extending cast that depends on `remaining` being bounded.
**Fix:** Replace bare `as` with `usize::try_from(...)` / `isize::try_from(...)` and document the bounds at each cast. Where the cast is genuinely safe (e.g. small numeric deltas in `usize`), `cast_lossless` or an `i64::from(x)`-style explicit conversion still reads better than bare `as`.

### No issues found in:
- **Display / FromStr as footguns**: each `Display`/`FromStr` impl I looked at is single-purpose (`SessionId` hex, `Filter` DSL, etc.). None double-duty as both an API and a config format.
- **Unsafe blocks**: none in the codebase.
- **Wildcard match arms**: none in the non-test paths; engine and filter spell out every variant.
- Most newtypes (`SourceId`, `LogStreamId`, `BookmarkId`, `Pid`, `Hostname`, `LoggerName`) are well-shaped.

### Summary
6 blocking issues, 10 suggestions.

The blockers cluster in two areas: synchronization between `RenderOpts` and `LogStream`'s mirrored fields (Findings 1, 2), and `Vec` collections that should be keyed (Findings 4, 5). Fixing those would eliminate three distinct classes of silent-failure mode and shrink the comment-enforced invariants in the codebase.

The suggestions are mostly opportunistic strengthenings that would compound over time — `Polarity` over `negated: bool`, `ByteLen` over `u64`, `LineIdx`/`EventIdx` over bare `usize`. None are individually urgent.
`````
