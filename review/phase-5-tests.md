# Phase 5 — Test review

The codebase has roughly 590 `#[test]` functions, distributed
roughly:

| File              | Tests | Notes                                |
|-------------------|------:|--------------------------------------|
| `bin/seer.rs`     |   254 | keymap + dialog + render integration |
| `filter.rs`       |    60 | DSL parse + predicate semantics      |
| `source.rs`       |    38 | metadata + forward/backward query    |
| `streamview.rs`   |    29 | search, anchor, scroll, slide         |
| `engine.rs`       |    27 | merge order, resolve_position, cursor |
| `engine/merge.rs` |    26 | stepper, buffer trim, out-of-order   |
| `summary.rs`      |    24 | histogram + bucket sizing            |
| `session_store.rs`|    22 | atomic write, list, match            |
| `bin/seeit.rs`    |    21 | session mode + cli plumbing          |
| `seeit_target.rs` |    20 | resolver semantics                   |
| `render.rs`       |    19 | column layout                        |
| `tests/seeit_session.rs` | 16 | seeit ↔ session integration      |
| `save_policy.rs`  |     9 | debounce state machine               |
| `tests/session_lifecycle.rs` | 7 | save/load/discover/resume         |
| `session.rs`      |     6 | round-trip + add/remove bookmark     |
| `event.rs`        |     5 | level / parse / duplicate keys       |
| `tests/session_schema.rs` | 1 | tripwire                          |

Findings below are conservative.  Most of the suite reads as
intentional; the patterns I flag are duplicate coverage that would
collapse naturally if the Phase 3/4 cleanups landed, plus a small
number of tests whose value is marginal.

## A. Tests that exist mostly because of the field structure flagged in earlier phases

### A1 — Five `show_X_persists_into_session_round_trip` tests

`bin/seer.rs:8829, 8880, 8954, 9152, 9170` each:
1. Construct an `App`, flip the toggle via key,
2. `serde_json::to_string` the session,
3. `serde_json::from_str` it back,
4. Assert the field has the toggled value.

They're useful as long as the `LogStream` schema mirrors `RenderOpts`
field-by-field (Phase 3 blocker — the destructure fix); the moment a
new `RenderOpts` knob is added, a *new* test file has to be added too,
and there's no compiler check that this happened.  With the
destructure fix in `set_render_opts`/`render_opts`, **one** test that
mutates every field and round-trips through serde would cover the
class.  Today's five tests are fine; they'll just multiply unless the
class is collapsed.

### A2 — Eleven tests for `Predicate`'s `negated` flag

`filter.rs:882-1057` is 11 tests anchored on the `negated: bool` field
(matching, parsing, display, serde-default).  Each variant
(`LevelEquals`, `FieldEquals`, `MsgMatches`, `SourceIdMatches`) gets
its own pair of "matches when set" and "parses with bang" tests,
plus a serde default test.

If the field becomes a `Polarity` enum (Phase 3 suggestion) and serde
no longer has a "default false" knob to test, two tests fold away:

- `serde_payload_without_negated_field_defaults_false` (:1033)
  becomes vacuous when the field is mandatory.
- `display_negated_forms` and `display_then_parse_round_trip_negated`
  collapse into the existing `display_then_parse_round_trip` —
  there's no longer a reason for negated forms to have their own
  round-trip test.

Until the type cleanup, the tests carry their weight.

### A3 — Two `ParseStats` test surfaces

(Restated from Phase 2.)  `bin/seer.rs` carries its own `ParseStats`
and tests it indirectly through the per-tab counter assertions in
the render tests; `streamview.rs` carries the richer struct and
tests it in isolation.  If the binary's type is removed (Phase 2
finding 6), no binary-side test goes away — every assertion is about
behavior, not the struct shape.  Worth restating: nothing here is
test debt, just a duplicate type that drags some tests' assertions
along.

## B. Tests with marginal value

### B1 — `release_events_are_ignored` (bin/seer.rs:5796)

Sends a synthetic `KeyEvent` with `KeyEventKind::Release` and asserts
the App does not move.  Tests that one early-return at the top of
`handle_key`.  Marginal because it's testing a single boolean check,
but the crossterm release/repeat distinction is a real source of
historical bugs, and the test is one line.  Keep.

### B2 — `dialog_keys_do_not_quit_or_scroll` (bin/seer.rs:5929) and `seeit_command_dialog_ignores_random_keys` (:10196)

These two tests assert that, with a dialog open, random keystrokes
don't escape to the underlying view.  The same assertion is implicit
in every dialog-content test (which would fail if scrolling happened
under the dialog) — but the explicit form is a useful regression
guard if a new dialog forgets to swallow keys.  Keep.

### B3 — `q_opens_quit_confirmation` + `esc_does_not_open_quit_confirmation` + `ctrl_c_does_not_open_quit_confirmation`

(`bin/seer.rs:5583, 5591, 5603`.)  Three tests for the
quit-confirmation key.  The two `does_not_open` tests are negative
assertions — that `esc` and `ctrl-c` *don't* fall into the quit
prompt.  Documented intent in the test comments.  Keep, they pin
down an explicit policy.

## C. Tests I think can be removed or merged

### C1 — `serde_payload_without_negated_field_defaults_false` (filter.rs:1033)

Tests `#[serde(default)]`.  This is serde's own behavior.  The
test's value is "we documented serde(default) on this field" — a
schema test, not a behavior test.  If the type becomes `Polarity`
(no default), this goes away.  If it stays, the test is testing the
*serde derive*; consider deleting and trusting serde.

### C2 — `column_chunks_handles_empty_and_unicode` (bin/seer.rs:6534)

The function `column_chunks` is well-defined and the test covers
the documented edge cases (empty input, width 0, multi-byte chars).
Useful.  But its sibling `wrap_dialog_text_handles_empty_and_chunking`
(:6225) overlaps significantly — both exercise the same wrap helper
in similar shapes.  Could merge.

### C3 — `cant_scroll_above_top` (seer.rs:5766) + `cant_scroll_below_bottom` (:5775) + `small_content_clamps_to_zero` (:5785)

Three tests for boundary-clamping in scroll.  Each is two assertions.
Could fold into one parametrized test (or one body with three
scenarios).  This is purely a readability call — they're each
self-explanatory today.

### C4 — Many `..._inserts_at_cursor`, `..._backspace_deletes_char_before_cursor`, `..._left_right_move_cursor`, `..._delete_removes_char_after_cursor`, `..._ctrl_u_kills_to_start_of_line` etc. across filter, search, rename dialogs

These exercise the `LineEditor` once per dialog.  Each dialog uses
the same `LineEditor` (`bin/seer.rs:4163`); the editor itself isn't
tested directly, only through the dialogs.  Consider extracting a
`LineEditor` test module with one set of tests for editor semantics,
then drop the per-dialog duplicates and keep a single
"dialog-passes-through-to-editor" test per dialog kind.

Counted: I see roughly 12-15 tests that exist in two or three
parallel dialog families (filter, search, rename).

### C5 — `format_session_list_empty_returns_friendly_message` (bin/seer.rs:9675)

Tests that the empty-list string is `"No saved sessions."` (or
whatever the friendly form is).  Locks in a specific
user-facing string.  Sensible if the string is policy; a marginal
test if the string is just whatever the function happens to emit.

### C6 — Three `t0()` helper functions in `save_policy.rs`

(Not a test problem, but the file has its own clock-fake.)  This
is fine — `save_policy` is a pure state machine and the helper is
local.

## D. Possible coverage gaps

These would be useful additions, not removals:

### D1 — No test directly covers `Engine::cursor_for_position`'s same-time-ordinal branch

`engine.rs:202-225` carefully handles the `in_same_time_group`
walk-off case.  The existing test `cursor_for_position_distinguishes_same_time_ordinals`
(`:1315`) covers the happy path; nothing tests the "we walked off
the group without finding the requested ordinal" return-None branch.

### D2 — No property test for "filter is conjunction"

The doc on `Filter::matches` says it returns true iff every predicate
accepts.  The existing `filter_is_conjunction` (filter.rs:760) tests
a single hand-crafted instance.  A property test that generates
random predicates and asserts `f.matches(e) == f.predicates().all(...)`
would be a few lines via `proptest` and would catch any future
short-circuit/precedence bug.

### D3 — No test that `SourceMetadata::excludes_all` and per-line filter agree

CLAUDE.md explicitly flags this risk:

> Index correctness vs. speed.  The substring/regex `time` extractor
> must agree with the full parser for the same line.  Add a property
> test: pick random parsed entries, re-extract via the index path,
> assert equal.

The codebase has `metadata::excludes_all` doing whole-source pruning
for `name`/`hostname` equality, but the "they agree" property isn't
tested.  Worth adding when SMF / CockroachDB formats land.

## Summary

- The bulk of the 590 tests look intentional.  The bin/seer.rs
  keymap tests in particular are appropriate for the binary's
  surface — every test exercises a real input.
- The duplicate coverage clusters around two type-safety holes
  flagged in Phase 3: the `negated: bool` field on `Predicate` and
  the `RenderOpts` ↔ `LogStream` field-by-field mirror.  Both
  cleanups would collapse 5-15 tests each as a side effect.
- A handful of tests test serde itself (`serde(default)` defaults);
  consider trusting serde and deleting.
- A few tests duplicate dialog interactions of the same underlying
  `LineEditor`; consolidating would shrink the test count without
  losing coverage.
- Coverage gaps are minor; D1/D2/D3 are nice-to-haves rather than
  must-haves.

No tests appear to be "mock-only" in the harmful sense.  The
`with_rows`/`with_events` test constructors (`bin/seer.rs:3679,
3693`) bypass the engine, but they're a deliberate test seam:
keymap and rendering tests have no need for real engine output,
and the engine is tested elsewhere.
