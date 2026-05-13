# Plan: review follow-ups

A flattened, deduped list of every actionable item from `review/phase-1`
through `review/phase-5`.  Ordered roughly easiest-first — the user
expects to discuss each item before it lands, so items that involve
the smallest mechanical change come earliest.

References use the form `P{n} §{key}` to point back to the originating
review file (e.g. `P3 §B1` = phase-3-type-safety.md, blocking finding
about `LogStream::render_opts`).  Items flagged in multiple phases are
listed once with all references gathered.

## Status

| Field          | Value                                              |
|----------------|----------------------------------------------------|
| Items closed   | 31 / 45                                            |
| Current item   | —  (pick the first unchecked below)                |
| Last updated   | 2026-05-12                                         |
| Notes          | Item 1: renamed to `FETCH_BATCH_SIZE` and re-exported through `engine`; dead `LONG_OP_BATCH_SIZE` doc link replaced with a pointer to `StreamView::ensure_window_step` where the small-batch values actually live.  Item 2: each of the three sites restructured to avoid the unwrap entirely — `try_save_now` collapses the redundant `is_none` check into a single `if let`; `push_tab` computes `render_opts` before the insert takes ownership; bookmark navigation fuses the bookmark→stream and stream→filter lookups into one `find_map` so only a single `.expect` (for the joint invariant) remains.  Item 3: `render_opts` destructures `self` (binding render-related fields, `_`-ing `id`/`name`/`filter`) and `set_render_opts` destructures `opts`; adding a new `RenderOpts` field now fails to compile in both directions until propagated.  Item 4: `SessionId` and `SessionIdParseError` moved from `session_store` to `session`; `session_store` now imports them from `session` (cycle broken).  `schema_id` updated from `seer::session_store::SessionId` to `seer::session::SessionId`; schema-fixture test still passes since the fixture doesn't `$ref` it.  `seeit_target` and `lib.rs` re-exports adjusted; `session_store` lost its unused `serde`/`fmt`/`FromStr`/`uuid` imports.  Item 5: local `ParseStats` deleted; `streamview::ParseStats` now flows through both the streamview-rendered and summary-finalize paths.  Lib re-export renamed from `StreamViewParseStats` to plain `ParseStats` (defensive alias no longer needed).  Summary path sets `bytes = walked_bytes = bytes_read` (option A — preserves today's status-line numbers); item 45 added at the end of the list to consider tracking filter-matching bytes separately later (option B).  Item 6: `seeit_target.rs` renamed to `view_target.rs` via `git mv`; module and re-export updated in `lib.rs`; no other call sites referenced the old path.  Item 7: new `BUNYAN_LEVELS: &[(u8, Level)]` near the `Level` enum is the single source of truth; `as_bunyan_number`, `TryFrom<u8>`, and the `JsonSchema` impl all walk it.  Trade-off: `as_bunyan_number` lost its exhaustive match (so a new `Level` variant won't compile-error there if forgotten in the table), but the existing `level_round_trip_numbers` test enumerates every variant and would fail on a missing entry.  Item 8: six (not five — the followup miscounted) `active_*` helpers collapsed into one `active_stream(&self) -> &LogStream`; callers pluck the field they want.  Skipped the proposed `_mut` sibling: the codebase mutates streams via `streams.remove → mutate → insert_unique`, not `get_mut`, so an `active_stream_mut` would have no caller today.  Item 9 (path B chosen): `negated: bool` on the four polar Predicate variants replaced with `form: Form` where `Form::Affirmed` / `Form::Negated`.  `Form::applied_to(condition)` replaces the XOR at every match site.  Bumped `CURRENT_SESSION_VERSION` 4→5 and refreshed `tests/fixtures/session.schema.json`.  No migration shim — old session files no longer load (pre-1.0).  Item 39 collapsed at the same time: `serde_payload_without_negated_field_defaults_false` test deleted, since its premise (the `#[serde(default)]` on `negated`) is gone.  Item 10: `forward_eof`/`backward_eof` on `StreamView` collapsed into a single `DirectionalEof { forward, backward }` field.  No `Direction`-indexed accessors added — every current caller has the direction in hand at compile time; the doc comment notes the one-line addition if a future parameterized caller needs them.  Item 11: deleted `streamview::TimeDir` (an internal duplicate of `source::Direction`) and reused the existing `Direction`; no other files referenced `TimeDir`.  Item 12: replaced five opaque `bool` parameters with named enums.  `format_time` now takes `ShowDate { Yes, No }` (with `impl From<bool>` for the `opts.show_date` bridge).  `Tab::advance_time` / `Tab::time_anchor_idx` reuse the existing `source::Direction`.  `Tab::jump_to_match` (and its private helpers, plus the `SearchOp` field and `StreamView::search_step{,_with_budget}` chain it threads into) take `SearchAnchor { Include, Skip }`.  `SourceWindow::set_eof` takes `EofMark { Reached, Cleared }`.  All `/* exclusive = */`, `/* forward = */`, `/* show_date = */` call-site comments are gone.  Item 13: new `ByteLen(u64)` newtype paired with `ByteOffset`, with `#[serde(transparent)]` so persisted shapes stay bare `u64`s on disk (schema fixture unchanged, no version bump).  Arithmetic operators encode the relationships: `Len + Len → Len`, `Offset + Len → Offset`, `Offset += Len`, `Offset - Len → Offset`, `Sum<Len> → Len`, plus `Len::saturating_sub`.  Fields converted: `QueryRecord.length`, `QueryBatch.walked_bytes`, `MergeRecord.length`, `BufferedRecord.length` (internal), `Stepper::walked_bytes`, `EventStream::bytes_read`, `Engine::filtered_total_bytes` return, `ParseStats.{bytes,walked_bytes}`, and all of `SummaryOp` / `SearchOp` / `SeekOp`'s byte counters.  Several `ByteOffset::from(offset.get() + length)` constructions simplified to `offset + length`.  Item 14: `TabIdx`, `EventIdx`, `LineIdx` newtypes added at the top of `bin/seer.rs`.  Vec element types tagged: `RenderedRows.event_for_line: Vec<EventIdx>` (indexed by line position), `RenderedRows.first_line_for_event: Vec<LineIdx>` (indexed by event position); the LineIdx↔EventIdx transposition that the review called out as the prime risk is now a compile error.  `Tab.viewport_top: LineIdx`, `Selection.event_idx: EventIdx`, `Tab::max_top` returns `LineIdx`, `Tab::time_anchor_idx` returns `Option<EventIdx>`, `Tab::last_line_for_event` takes `EventIdx → LineIdx`.  `LongOp::{BuildSummary,Search,Seek}::tab_idx: TabIdx` plus `App::pending_summary_builds: VecDeque<(TabIdx, Filter)>`; constructors take `TabIdx`.  `App.active` stayed `usize` (it touches ~250 sites; wrapping at the LongOp boundary kept the diff bounded).  Helper methods on the newtypes are intentionally minimal: `get() -> usize`, `Display`, plus `PartialEq<usize>` / `PartialOrd<usize>` so test assertions like `assert_eq!(tab.viewport_top, 3)` and `assert!(top > 0)` don't need wrapping at every callsite (the typed slots in the structs are still strictly typed).  Item 15: bare `as usize` / `as isize` casts in `StreamView::scroll_lines`, `search_step_backward`, and `move_selection` replaced with `usize::try_from(...).expect("...")` / `isize::try_from(...).expect("...")` so each conversion now panics on overflow rather than silently wrapping.  Local `cap_isize` closure in the two functions that repeat the usize→isize cap keeps the conversions terse.  `scroll_lines`' backward branch dropped its `(line as isize + remaining) as usize` round-trip by working in unsigned magnitudes via `remaining.unsigned_abs()` once the sign is known.  The two remaining `viewport_height as usize` casts (lines 632, 740, 1678, 1683) are u16→usize widening; always lossless, left untouched.  Item 16: `Cursor` moved from `engine/merge.rs` to a new top-level `src/position.rs`.  `engine.rs` re-exports it via `pub use crate::position::Cursor` so `crate::engine::Cursor` still resolves and nothing else had to move.  `Stepper::cursor()` was a friend of the private `offsets` field; it now calls `Cursor::with(...)` for construction instead.  `ByteOffset`/`ByteLen` stay in `source.rs` for now — the "alongside ByteOffset" framing in this item is descriptive, and moving them creates a `source` ↔ `position` cycle whose right resolution belongs with item 31 (the `SourceId` placement decision).  Plain rename: the on-disk JSON shape is unchanged (`Cursor` is still `#[serde(transparent)]` around the same `BTreeMap<SourceId, ByteOffset>`), so no schema bump.  Item 17: `LogStreamPosition` moved from `stream.rs` to `position.rs` (sits beside `Cursor`).  `engine.rs` switched its `use crate::stream::LogStreamPosition` to `crate::position::LogStreamPosition`; `lib.rs`'s re-export moved from the `stream::{...}` line to a new `pub use position::LogStreamPosition`.  `LogStream` itself stays in `stream.rs` (it's the per-session view-configuration object, not a low-level position).  No schema impact: `LogStreamPosition` isn't referenced by name in the persisted session shape — it appears inside [`Bookmark`] but only as inlined fields, not by JsonSchema id.  Item 18: documented (not normalized) the absent-vs-zero semantics on `Cursor` with a new "Absent vs. zero" section in the type-level doc comment.  Normalizing on construction would require the type to know the full engine source set, which `Cursor` deliberately doesn't.  Today nothing observable depends on the distinction (bookmark dedup and session round-trip both preserve whatever shape the caller built), so the right call is to flag the trap for a future caller that wants logical-equality semantics — they should walk the shared key set via `Cursor::get` (which folds absent to `ByteOffset::ZERO`) rather than rely on derived `==`.  Item 19: `Session.sources` switched from `Vec<SessionSource>` to `IdOrdMap<SessionSource>`; `SessionSource` impls `IdOrdItem` keyed by `id`.  Same `#[schemars(with = "Vec<SessionSource>")]` trick used for `Session.streams` keeps the JSON-array on-disk shape — no schema version bump.  Side effect: `Session` grew a bit, which pushed clippy's `large_enum_variant` over its threshold on `StartupChoice`, so `StartupChoice::Resume(Session)` became `Resume(Box<Session>)`.  `build_session_sources` returns `IdOrdMap<SessionSource>`; eight test fixtures' `.sources.push(...)` calls became `.sources.insert_unique(...).unwrap()`.  Test-only assertion in `build_session_sources_canonicalizes_and_stats_each_file` switched from `sources[N]` indexing to iteration since `IdOrdMap` orders by id (which for file sources is the canonical path), so the ordering observation still holds.  Schema fixture regenerated to pick up the doc-comment paragraph on `sources` — the array shape itself is unchanged.  Item 20: `Session.user_bookmarks` inner `Vec<Bookmark>` switched to `IdOrdMap<Bookmark>`; `Bookmark` impls `IdOrdItem` keyed by `id`.  `add_bookmark` now `insert_unique`s (panics on duplicate id — fine since bookmark ids are minted fresh at creation time).  `remove_bookmark` collapsed from a `position`-and-`remove` linear scan to a single O(log n) `IdOrdMap::remove`.  `#[schemars(with = "BTreeMap<LogStreamId, Vec<Bookmark>>")]` preserves the on-disk JSON shape — no version bump.  Schema fixture regenerated for the new doc-comment paragraph on `user_bookmarks`; the array-of-bookmarks per stream is unchanged on disk.  Tests that did `bms[0].field` switched to `bms.iter().next().unwrap().field`, since `IdOrdMap`'s iteration order is by id (uuid bytes), not insertion.  Item 21: `Engine::{stepper, stepper_with_batch, stepper_with_bounds}` collapsed to `Engine::{stepper, stepper_with}`, where `stepper_with` takes a new `StepperOptions { batch_size, max_walks_per_fill: Option<usize> }`.  Default `stepper(filter, cursor)` shortcut kept untouched so 10+ callers don't churn.  Matching trio on `Stepper` (`new` / `with_batch_size` / `with_bounds`) collapsed to `new` (defaults) + `with_options(...)`.  The three custom call sites in `streamview.rs` (`extend_*_small_batch` × 2 and `extend_backward_batch_n`) now construct `StepperOptions` with explicit field syntax; the per-fill walks budget that used to be a `usize` parameter is now wrapped in `Some(...)` at the call site (matching the underlying `Option<usize>` on `Stepper`).  Item 22 (skipped above) bundled `Stepper::step_backward_n` consideration — separate item, also untouched.  Net diff is `-10` lines across three files even after adding the `StepperOptions` definition.  Item 26: new `enum Row { Event(EngineEvent), Error(String) }` replaces the `Vec<Option<EngineEvent>>` shape on `RenderedRows.events` and `Tab.events`.  Construction in `materialize_streamview` pushes `Row::Event { position, event }` / `Row::Error(err.to_string())` (with `formatted` still receiving the same display string as before — duplication is intentional; the `Row::Error` payload is the source of truth, `formatted` is the cached rendering).  Three real read sites updated: `Tab::time_anchor_idx` and `Tab::advance_time` switched from `e.as_ref().map(...)` / `.filter(...)` chains to `matches!(e, Row::Event(_))` and `match { Row::Event(ee) if ... }`; `App::commit_selection` `let Some(Some(ee)) = ...` became `let Some(Row::Event(ee)) = ...`.  Test constructor `App::with_events`'s public signature kept (`Vec<Option<LogEvent>>`); internally it zips with `formatted` and maps `None` → `Row::Error(formatted[i].clone())`, so the five existing call sites and `with_rows` are untouched.  `Row::Error`'s `String` payload has no read site today, but it's carried (not unit) so a `Row::Error` row literally can't exist without its message attached — that pairing is what the enum exists to enforce.  Marked `#[allow(dead_code)]` with a doc note pointing future readers (a "show error details" dialog or a refined exclude-mode notice) at the payload.  Chose a domain enum over `Result<EngineEvent, String>` because `Result`'s semantic weight ("operation failed") doesn't match here — materialization always succeeded, the slot just describes an error record — so `?` / `.ok()` / `.collect::<Result<_,_>>()` shouldn't be on the table for callers.  Plus future variants (analyzer-injected synthetic records, orphaned-plaintext rows from CLAUDE.md's roadmap) can be added without a `Result` → enum migration.  Item 29: `Predicate` split into `EventPredicate` (LevelAtLeast / LevelEquals / FieldEquals / MsgMatches / TimeBound) and `SourcePredicate` (SourceIdMatches), with a wrapper `Predicate { Event(EventPredicate), Source(SourcePredicate) }` carrying the persisted shape so DSL ordering still round-trips through a single `Vec<Predicate>`.  `EventPredicate::matches(event)` is now exhaustive — no more `Self::SourceIdMatches { .. } => true` "always-true" arm at the per-variant level — and `SourcePredicate::matches(source_id)` mirrors it.  `Filter::matches` / `Filter::matches_source_id` still skip the other kind via a `match` on the wrapper, but that's the explicit conjunction-skip pattern, not a buried no-op.  `From<EventPredicate>` and `From<SourcePredicate>` for `Predicate` keep `.into()` ergonomic at construction sites.  `CURRENT_SESSION_VERSION` bumped 5→6 and the schema fixture refreshed (each on-disk predicate now wraps as `{"Event": {...}}` / `{"Source": {...}}`); no migration shim, consistent with item 9's call.  External callers updated: `SourceMetadata::excludes_all` destructures through the wrapper; `bin/seer.rs`'s exclude-mode predicate construction (and the one test that builds `MsgMatches`) now go through `EventPredicate::...into()`.  `lib.rs` re-exports `EventPredicate` and `SourcePredicate` alongside `Predicate`.  Item 38 closed at the same time: the three tests that P5 §A2 named for folding (`display_negated_forms`, `display_then_parse_round_trip_negated`, `source_id_predicate_is_noop_at_event_level`) are deleted.  The first two collapse into `display_canonical_forms` and `display_then_parse_round_trip` (which I extended to exercise both `Form::Affirmed` and `Form::Negated` for every applicable variant).  The third is obsolete by construction — `SourcePredicate::matches` now takes a `SourceId`, not an `&Event`, so the misuse it guarded against (calling `matches(event)` on a source predicate and silently returning `true`) won't compile.  The other eight per-variant negation tests P5 §A2 mentioned aren't redundant — they pin down behaviors (e.g. `field_equals_negated_missing_field_matches`) that the per-variant inversion test alone wouldn't catch — so they stay.  Item 31: chose option "break the cycle."  Moved `SourceId`, `ByteOffset`, and `ByteLen` from `source.rs` to `position.rs`, making `position.rs` the leaf primitive-types module.  Item 16's notes had explicitly anticipated this scope — moving only `SourceId` would have left a fresh `source ↔ position` cycle in place of the closed `filter ↔ source` one, since `position.rs` was already importing `ByteOffset` from `source.rs`.  Pulling the three byte/id newtypes together leaves a clean one-directional graph: `position` (leaf) ← `source`, `filter`, `summary`, `session`, `view_target`, `engine{,/merge}`, `streamview`, plus tests.  Inside `source.rs`, the few `ByteLen(n)` / `ByteOffset(n)` literal-construction sites (private fields aren't accessible from outside the defining module any more) switched to `ByteLen::from(n)` / `ByteOffset::from(n)`, which the existing `derive_more::From` impls already provide.  `derive_more::AsRef` and `std::ops::{Add, AddAssign, Sub}` imports migrated to `position.rs` along with the types; `source.rs` shed its `schemars::JsonSchema` import (no longer used now that the types live elsewhere).  Stale doc reference inside `SourceId`'s comment (`crate::stream::LogStreamPosition` → `crate::position::LogStreamPosition`, untouched since item 17) fixed at the same time.  Zero schema impact: the on-disk shapes are unchanged (`#[serde(transparent)]` newtypes serialize the same regardless of which module defines them), so no version bump and `session_schema_matches_fixture` still passes.  Side benefit: an `EventPredicate::matches(_)` over a `SourcePredicate` still won't compile (the type split from item 29 enforces this regardless), but `filter.rs` no longer needs to depend on `source.rs` at all — its only previous use was for `SourceId`, now imported from `position.rs`.  Item 37: five `show_X_persists_into_session_round_trip` tests collapsed into one `all_render_opts_persist_into_session_round_trip` that flips every `RenderOpts` dimension to a non-default value (`shift('F')` / `shift('R')` / `shift('D')` for the three with keybindings; the display-fields dialog for `show_pid` / `show_name` / `hostname_display`) and round-trips the session.  Item 3's destructure-on-copy in `LogStream::render_opts` / `set_render_opts` is what enforces propagation for newly-added fields; this consolidated test now just confirms the persistence layer preserves whatever the stream is carrying, so per-field tests were over-coverage.  Net 5 → 1 test, ~50 lines deleted.  Item 42: new test `cursor_for_position_returns_none_when_ordinal_past_group` covers the `else if in_same_time_group { return None }` arm in `Engine::cursor_for_position` (`src/engine.rs:194-197`).  Builds a single-source fixture with two same-time events at `t(10)` and a later event at `t(20)`, constructs a position at `(source, t(10), ordinal=2)` (one past the last event in the group), and asserts the resolver returns `None`.  Walks the same code path that a corrupted bookmark or schema mismatch would hit; the assertion would fail if the branch ever flipped to `Some`. |

### How this state works

Same convention as `test-plan.md`.  Each item is a checkbox.  When the
user asks for "the next item", pick the first unchecked one in this
list, discuss it briefly, land the change, tick the box, and update
the **Items closed** and **Last updated** rows.  If an item splits
into sub-tasks during discussion, add them as nested checkboxes under
the parent — don't silently absorb scope.

If discussion lands on "skip this one" or "fold into item N", record
that in **Notes** (with a one-line reason) and tick the box; the
record matters more than the binary outcome.

---

## Quick mechanical wins

Each of these is a single-file or single-concept change with no
genuine design choice to make.

- [x] **1. Dedupe `BATCH_SIZE = 64`.**  Define once (e.g.
      `pub const FETCH_BATCH_SIZE: usize = 64;` at the engine root)
      and import in both call sites.  *Refs:* P1 §6, P3 §S2, P4 §C1.
      *Affects:* `src/engine/merge.rs:48`, `src/streamview.rs:39`.

- [x] **2. Replace bare `.unwrap()` with `.expect("…")`** at three
      sites in `bin/seer.rs` so they match the file's documented-
      invariant idiom.  *Refs:* P3 §S10.  *Affects:* `src/bin/seer.rs:2138, :2220, :3169`.

- [x] **3. Destructure `LogStream::render_opts` and `set_render_opts`.**
      Pattern-bind every `RenderOpts` field so adding a new dimension
      fails to compile until propagated.  *Refs:* P3 §B1, P5 §A1.
      *Affects:* `src/stream.rs:195, :210`.  *Enables:* item 37.

- [x] **4. Move `SessionId` from `session_store` into `session`.**
      Breaks the `session ↔ session_store` cycle.  Pure rename.
      *Refs:* P2 §F2, P4 §B3.  *Affects:* `src/session_store.rs`,
      `src/session.rs`.

- [x] **5. Drop `bin/seer.rs`'s local `ParseStats` in favor of
      `streamview::ParseStats`.**  The streamview type is a strict
      superset (it carries `walked_bytes`).  *Refs:* P2 §F6, P4 §A3,
      P5 §A3.  *Affects:* `src/bin/seer.rs:683` and call sites.

- [x] **6. Rename `seeit_target.rs` → `view_target.rs` (or
      `resolved_view.rs`).**  The file resolves a saved view for both
      `seer` and `seeit`; the `seeit_` prefix misreads as binary-
      specific.  *Refs:* P2 §F9.  *Affects:* `src/seeit_target.rs`.

- [x] **7. Centralize the bunyan-level numeric mapping.**  One
      `const TABLE: &[(u8, Level)]` walked by both `as_bunyan_number`
      and `TryFrom<u8>`, with `JsonSchema` reading from it too.
      *Refs:* P3 §S3.  *Affects:* `src/event.rs:152, :196, :243`.

- [x] **8. Consolidate `App`'s five `active_*` helpers** into one
      `active_stream(&self) -> &LogStream` (and a `_mut` sibling).
      Callers pluck the field they want; the single
      `.expect("stream exists")` lives in one place.  *Refs:* P4 §A9.
      *Affects:* `src/bin/seer.rs:2983, :2991, :2999, :3007, :3014`.

## Targeted type-safety improvements

Each is bounded in scope but touches a few files.  Where an item
collapses follow-on tests automatically, that's noted.

- [x] **9. `Polarity` enum replaces `negated: bool` on `Predicate`.**
      `Polarity::Affirm` / `Polarity::Deny`; matching is a
      `match polarity` instead of an XOR.  Eliminates the
      `/* negated = */ false` comments at every DSL parse site.
      *Refs:* P3 §S1, P5 §A2.  *Affects:* `src/filter.rs:99, :181, :290, :294, :309, :312, :323`.
      *Enables:* item 38, possibly item 30.

- [x] **10. Replace `forward_eof: bool` / `backward_eof: bool` with a
      `Direction`-keyed structure.**  Either a small `DirectionalEof`
      wrapper or `EnumMap<Direction, bool>`.  *Refs:* P3 §S6, P4 §C3.
      *Affects:* `src/streamview.rs:256, :257` and all read sites.

- [x] **11. Delete `streamview::TimeDir`** and reuse the existing
      `source::Direction`.  *Refs:* P4 §C3.  *Affects:*
      `src/streamview.rs:200` and call sites.

- [x] **12. Replace opaque `bool` function parameters with named
      enums.**  Cases: `format_time(_, show_date: bool)`,
      `Tab::advance_time(forward: bool)`,
      `Tab::time_anchor_idx(prefer_forward: bool)`,
      `Tab::jump_to_match(_, exclusive: bool)`,
      `SourceWindow::set_eof(_, value: bool)`.  *Refs:* P3 §S7.

- [x] **13. Introduce `ByteLen(u64)` newtype.**  Same affordances as
      `ByteOffset`.  Type-checks the combination
      `ByteOffset + ByteLen → ByteOffset`.  *Refs:* P3 §S4, P4 §C2.
      *Affects:* `QueryRecord.length`, `MergeRecord.length`,
      `QueryBatch.walked_bytes`, `ParseStats.bytes`,
      `EventStream::bytes_read`, `LongOp.*total_bytes`.

- [x] **14. Introduce `TabIdx`, `EventIdx`, `LineIdx` newtypes.**
      Catches the inevitable transposition between
      `event_for_line` (LineIdx → EventIdx) and
      `first_line_for_event` (EventIdx → LineIdx) at compile time.
      *Refs:* P3 §S5, P4 §C2.  *Affects:* `src/bin/seer.rs:671,
      :672, :1014, :1097, :1180`.

- [x] **15. Replace bare `as usize` / `as isize` casts in StreamView
      navigation** with `try_from(...)` + comments justifying each
      remaining cast.  *Refs:* P3 §S11.  *Affects:*
      `src/streamview.rs:1108-1178, :1556-1616, :1650`.

## Module reshapes

Small placement changes that clean up the dependency graph.

- [x] **16. Move `Cursor` out of `engine/merge.rs`** into a new
      top-level home (e.g. `position.rs`) alongside `ByteOffset`.
      Today it's an internal helper that half the codebase persists.
      *Refs:* P2 §F3, P4 §B1.

- [x] **17. Move `LogStreamPosition` out of `stream.rs`** to sit next
      to `Cursor` (item 16).  `LogStream` itself stays.  *Refs:*
      P2 §F4, P4 §B2.

- [x] **18. Decide on `Cursor` absent-vs-zero semantics.**  Either
      normalize on construction so the map is dense, or document
      explicitly next to `PartialEq` that absence and zero compare
      unequal.  *Refs:* P3 §S8.  *Affects:* `src/engine/merge.rs:70`.

## Collection types

- [x] **19. `Session.sources: Vec<SessionSource>` → `IdOrdMap`.**
      `SessionSource: IdOrdItem` keyed by `id`.  Serde form stays a
      JSON array; no on-disk migration.  *Refs:* P3 §B4.
      *Affects:* `src/session.rs:199`.

- [x] **20. `Session.user_bookmarks` inner `Vec<Bookmark>` →
      `IdOrdMap<Bookmark>`.**  `add_bookmark` becomes
      `insert_unique`; `remove_bookmark` becomes an O(log n)
      lookup.  *Refs:* P3 §B5.  *Affects:* `src/session.rs:216, :255`.

## Stepper / merge API

- [x] **21. Consolidate Engine's three stepper constructors.**
      Either a `StepperBuilder` returned by `Engine::stepper(...)`,
      or one `stepper_with_bounds(...)` + named defaults.  *Refs:*
      P1 §2, P4 §A1.  *Affects:* `src/engine.rs:75, :92, :112` and
      the matching trio on `Stepper` in `src/engine/merge.rs`.

- [x] **22. ~~Drop or symmetrize `Stepper::step_backward_n`.~~**
      *Skipped.*  The one in-tree caller (`seeit --before N`) wants
      exactly the existing shape, and the asymmetry — backward
      bulk-walk has no forward sibling — reflects a real asymmetry
      in `seeit`'s output mode (it streams forward, so a "N records
      forward of cursor" helper would never get used).  Neither
      removing nor symmetrizing pays for itself.  *Refs:* P4 §A7.

- [ ] **23. Consolidate StreamView's five `extend_*` methods** into
      one or two methods parameterized by `(direction, batch_size,
      max_walks)`.  *Refs:* P4 §A8.  *Affects:* `src/streamview.rs:809,
      :849, :900, :905, :952`.
  - *Deferred 2026-05-12* after a closer look.  The five are
    `extend_forward_batch`, `extend_forward_small_batch`,
    `extend_backward_batch`, `extend_backward_small_batch`,
    `extend_backward_batch_n` (last is the default-batch backward
    implementation that `extend_backward_batch` wraps).  Each pair is
    the same body with cursor field, deque end, and step direction
    flipped; item 10's `DirectionalEof::{get,set}` covers the EOF
    flag, and a couple of helpers or `match dir` arms would cover
    cursor selection and `push_back`/`push_front`.  Two wrinkles to
    weigh before picking this up:
    1. EOF semantics differ.  Unbounded path sets `eof.forward` the
       moment `step_forward()` returns `None`; bounded path only sets
       it when `stepper.is_exhausted(dir)` reports true, since `None`
       from a bounded stepper can also mean "walks budget expired".
       `Option<max_walks>` is the natural signal, but the doc on the
       consolidated method has to spell the asymmetry out.
    2. `walked_bytes` is only accumulated in the bounded path today
       (no progress bar to feed in the unbounded one).  Probably
       worth always tracking after consolidation; cost is one add
       per call.
    The two `extend_*_until<F>` wrappers are a different shape and
    would stay as is.  ~120 → ~50 line body, plus call-site updates;
    low semantic risk since it's internal to `StreamView` and well-
    covered by tests.

- [x] **24. ~~Extract `OutOfOrderDetector` helper~~** used by both
      `SourceCursor::fill` and the stepper, until the two merges
      unify (item 35).  *Refs:* P4 §C5.
  - *Skipped 2026-05-12.*  The finding's premise turns out to be
    inaccurate: only `SourceCursor::fill` (the unfiltered
    `EventStream` path) does out-of-order detection.  The stepper
    has none — meaning a streamview-backed session silently emits
    records out of order when a source is unsorted.  Extracting a
    helper with only one caller would just introduce indirection;
    adding stepper-side detection is real work that item 35 (the
    big merge unification) would throw away.  The honest bridge is
    to do nothing here and let item 35 close both gaps together.
    If item 35 stalls and the silent-emit behavior becomes user-
    visible before then, revisit with the option-#1 plan from
    discussion (per-source detector hooked into `SourceWindow`).

## Design discussions

Each of these has multiple plausible answers; the right call depends
on tradeoffs the user will want to weigh.

- [ ] **25. `RenderOpts.show_raw` → `RenderMode` enum.**
      `RenderMode::Raw | Formatted { show_extras, show_date, hostname,
      show_pid, show_name }`.  Makes the "raw silently ignores other
      fields" hazard a compile-time impossibility.  *Refs:* P3 §B2.
      *Pairs with:* the `LogStream` mirror question (items 3 / 27).
  - *Deferred 2026-05-12* after discussion.  The naive enum reshape
    has a real UX cost: switching to Raw discards the formatted
    prefs (`show_extras`, hostname mode, etc.), and switching back
    resets to defaults.  Today's flat struct preserves prefs across
    mode toggles, which the user values.  Three paths when this is
    picked back up:
    1. *Skip.*  Keep the flat struct; accept the
       "raw silently ignores other fields" hazard at the consumer.
       Small, documented, well-localized.
    2. *Strict enum, accept the loss.*  Clean type, worse UX.
    3. *Strict enum at runtime, flat persistence on `LogStream`.*
       `RenderOpts` is the enum (`Raw | Formatted { ... }`).
       `LogStream` keeps the individual field bools alongside a
       `mode: RenderMode`; `LogStream::render_opts()` assembles the
       enum from the cached fields when in Formatted mode.  Toggling
       Raw → Formatted reads back the cached prefs.  More code, but
       satisfies both goals.  Effectively collapses item 33 (which
       asks the same `LogStream ↔ RenderOpts` design question) into
       this one — pick option 3 here and item 33 is decided too.

- [x] **26. `RenderedRows.events: Vec<Option<EngineEvent>>` →
      `Vec<Row>`** where `Row` is `Event(EngineEvent) | Error(String)`.
      Removes the parallel-`None`/error-string invariant.  *Refs:*
      P3 §B6.  *Affects:* `src/bin/seer.rs:669`.

- [ ] **27. `Tab.kind: TabKind` + `streamview: Option<StreamView>` →
      `TabContent` enum.**  `Stream { view, rendered }` /
      `Summary { rendered }`.  Eliminates the documented "iff"
      invariant.  *Refs:* P3 §B3.  *Affects:* `src/bin/seer.rs:1396`.
  - *Deferred 2026-05-12* after discussion.  Two entangled wrinkles
    push this past a clean one-shot landing:
    1. The "iff" stated in the doc is wrong in practice.  Test
       fixtures built via `App::with_events` / `App::with_rows` carry
       `kind == TabKind::Stream` with `streamview: None`, and
       production code already accommodates the third state with
       `if let Some(view) = tab.streamview ... else fallback` branches
       in `advance_time`, `scroll_lines`, `seek_active_to_end`,
       `seek_active_to_cursor`, and `jump_to_match`.  A two-variant
       enum forces a choice — either migrate the synthetic fixtures
       to a real engine (smallest enum, largest test surgery) or
       expose a third `SyntheticStream` variant in production code
       (smallest test surgery, uglier production matches).
    2. Item 28 ("Resolve `Tab` vs `StreamView` materialization") is
       going to revisit how rendered lines are stored.  The
       followup's `rendered: RenderedRows` payload assumes a unified
       sub-struct; today's `Tab` flattens those fields out, and which
       shape is right depends on what item 28 chooses for
       `StreamView` ownership of the flat lines.
    Right move is to land items 27 + 28 together once item 28's
    materialization decision is made, with the synthetic-fixture
    question answered as part of that bundle.  Picking up 27 alone
    risks a rewrite at the 28 boundary.

- [ ] **28. Resolve `Tab` vs `StreamView` materialization.**  Either
      move materialization onto `StreamView` (return a borrowed
      `&Materialized`), or have `StreamView` own the flat lines
      directly and drop the deque shape.  Today's "both" is the
      worst answer.  *Refs:* P2 §F5, P4 §A4.

- [x] **29. Split `Predicate` into `EventPredicate` and
      `SourcePredicate`.**  Removes the `Predicate::SourceIdMatches`
      "matches returns true" lie.  Pairs naturally with `Polarity`
      (item 9).  *Refs:* P1 §10, P4 §A10.  *Affects:* `src/filter.rs`.

- [ ] **30. `FieldName { Core(CoreField), Extra(String) }`** for
      filter field names.  Lets `SourceMetadata::excludes_all` match
      on a typed enum instead of comparing strings.  *Refs:* P3 §S9.
      *Affects:* `src/filter.rs:115, :220`, `src/source.rs:330`.

- [x] **31. Filter ↔ source cycle: accept or break.**  Either lift
      `SourceId` to a common types module (e.g. `position.rs` from
      item 16) so the cycle is broken at the type level, or accept
      the cycle (it's small) and document why.  *Refs:* P2 §F1.

- [ ] **32. Re-examine `MergeError(Arc<SourceError>)`.**  Today the
      `Arc` exists only because `std::io::Error` and
      `serde_json::Error` aren't `Clone`.  Alternative: store
      `Display` strings on merged records (raw line is already
      retained).  *Refs:* P4 §A6.  *Affects:* `src/engine/merge.rs:148`.

- [ ] **33. Tighten `LogStream` ↔ `RenderOpts` mirror long-term.**
      Item 3 fixes the silent-failure today via destructuring; the
      deeper choice is whether `LogStream` should embed
      `RenderOpts` directly (one schema bump + migration shim) or
      stay flat for per-field schema evolution.  *Refs:* P3 §B1, P4 §C4.

## Large refactors

- [ ] **34. Lift `LongOp` + `SummaryOp` + `SearchOp` + `SeekOp` into
      the library** (e.g. `engine::long_op`).  Makes them
      unit-testable without spinning up `App` and lets `seeit` reuse
      them for a future `--progress` mode.  *Refs:* P2 §F7, P4 §A5.
      *Affects:* `src/bin/seer.rs:939-1240` (~300 lines).

- [ ] **35. Unify the two k-way merges.**  One merged record type
      carrying source_id + offset + length + raw + event + position;
      `EventStream` becomes a thin wrapper around
      `Stepper::step_forward`; out-of-order detection lives in one
      place.  Largest single library cleanup.  *Refs:* P1 §1, P2 §F8,
      P4 §A2, P4 §C5.

- [ ] **36. Decide on splitting `bin/seer.rs`.**  10K lines, ~80
      top-level items, 2K-line `App` impl, 450-line `Tab` impl.
      Candidates to split out: input handling, rendering, dialog
      logic.  *Refs:* P1 §5.

## Test cleanups

Mostly automatic consequences of the type-safety changes above.

- [x] **37. Collapse the five `show_X_persists_into_session_round_trip`
      tests** into one that mutates every `RenderOpts` field.  Lands
      after item 3.  *Refs:* P5 §A1.

- [x] **38. Fold the eleven `Predicate` negated-flag tests** after
      `Polarity` lands (item 9).  *Refs:* P5 §A2.

- [x] **39. Delete `serde_payload_without_negated_field_defaults_false`.**
      Either redundant after item 9, or testing serde itself.  *Refs:*
      P5 §C1.  *Affects:* `src/filter.rs:1033`.

- [x] **40. ~~Merge `column_chunks_handles_empty_and_unicode` with
      `wrap_dialog_text_handles_empty_and_chunking`.~~**  Same wrap
      helper, two test bodies.  *Refs:* P5 §C2.  *Affects:*
      `src/bin/seer.rs:6225, :6534`.
  - *Skipped 2026-05-12.*  The "same wrap helper" framing is
    inaccurate: `column_chunks` (`src/bin/seer.rs:856`) counts
    characters and is used by the raw-mode render path;
    `wrap_dialog_text` (`:5553`) counts bytes with char-boundary
    backoff and is used by the dialog editor — and the dialog's
    cursor math at `:5520-5524` (`cursor / inner_width`,
    `cursor % inner_width`) assumes byte indexing to match.  A
    literal test merge is cosmetic (only the empty-input case is
    common; the column_chunks unicode test would fail if applied
    to `wrap_dialog_text`).  A real unification — fold
    `wrap_dialog_text` onto `column_chunks` — would also have to
    rework the cursor math to be char-aware, which is more work
    than this item scoped and might introduce a regression in
    dialog typing for multi-byte input.  Best left alone until
    multi-byte dialog input becomes a felt need.

- [ ] **41. Extract a `LineEditor` test module.**  Drop the
      per-dialog editor duplicates (insert / backspace / left-right /
      delete / ctrl-u across filter, search, rename); keep one
      "dialog passes through" test per dialog kind.  *Refs:* P5 §C4.

## Coverage adds

- [x] **42. Add test for `Engine::cursor_for_position`'s walked-off-
      group return-None branch.**  *Refs:* P5 §D1.  *Affects:*
      `src/engine.rs:202-225`.

- [ ] **43. Add a property test for `Filter::matches` being a
      conjunction.**  Random predicates; assert
      `f.matches(e) == f.predicates().all(...)`.  *Refs:* P5 §D2.

- [ ] **44. Add an agreement test between
      `SourceMetadata::excludes_all` and per-line filter.**  Worth
      adding once SMF / CockroachDB formats land, per the CLAUDE.md
      callout.  *Refs:* P5 §D3.

## Surfaced during follow-up work

- [ ] **45. Decide whether summary-build parse stats should report
      filter-matching bytes (option B from item 5's discussion).**
      Today, `SummaryOp` sums `rec.length` unconditionally into
      `bytes_read`, so the status line's "bytes" number for a summary
      tab under a selective filter reports walked bytes — closer to
      `walked_bytes` semantics than `bytes` in `streamview::ParseStats`
      terms.  Item 5 mechanically merged the two `ParseStats` types
      without fixing this (set `bytes = walked_bytes = bytes_read` in
      the summary path).  Doing it properly means tracking a separate
      filter-matching byte counter in `SummaryOp::advance` and
      installing it as `bytes` at finalize; the visible effect is the
      summary tab's status-line "bytes" number gets smaller for
      selective filters.  *Surfaced by:* item 5 discussion.
      *Affects:* `src/bin/seer.rs:1029-1071`.

---

## Items intentionally left off this list

- **Phase 5 §B1–B3 (release events, dialog key-swallow, quit
  confirmation).**  Phase 5 explicitly recommends keeping these; no
  action.
- **Phase 5 §C3, §C5, §C6.**  Style judgement calls the review says
  are fine as-is; the user can revive them as standalone items if
  they disagree.
- **Phase 2 §F10, all of Phase 4's "what looks well-shaped" list.**
  Positive findings; no work.
