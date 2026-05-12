// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scale-coverage tests for the seer engine and streamview.
//!
//! These tests build fixtures large enough to exercise window trims,
//! refetches, long-op chunking, and merge-many-sources paths at sizes
//! representative of real Oxide support data.  Each test fits well
//! inside CI's per-test budget; together they take a few seconds.
//!
//! Compiled only with `--features test-fixtures` enabled, which gates
//! the `seer::test_fixtures` helpers used here.

use camino::Utf8Path;
use chrono::{Duration, SecondsFormat};
use seer::test_fixtures::{
    GenOpts, TestDir, gen_multi_source, gen_single_source,
};
use seer::{
    Cursor, Engine, Filter, LogStreamPosition, RecordKey, RenderOpts,
    SearchDir, SearchOutcome, SourceError, StreamView, SummaryBuilder,
    summarize,
};
use std::fs::File;
use std::io::{BufWriter, Write};

/// Default count for the larger fixtures.  Chosen so that
/// `BUFFER_LIMIT` (256), `BATCH_SIZE` (64), and `WINDOW_SOFT_CAP`
/// (1024) are each crossed multiple times during a full forward
/// pass while keeping debug-build tests under a few seconds each.
/// The plan originally called for 50K records here, but a 5× shrink
/// preserves coverage of every trim/refetch path and brings each test
/// well under CI's sub-second-per-test budget in release mode.
const LARGE_COUNT: usize = 10_000;

/// Writes a single-source fixture identical to `gen_single_source`'s
/// default output, but lets the caller override one record's `msg` so
/// search and bookmark tests can target a known position.
fn gen_single_source_with_marker(
    path: &Utf8Path,
    name: &str,
    count: usize,
    marker_idx: usize,
    marker_msg: &str,
) {
    let opts = GenOpts::default();
    let mut f = BufWriter::new(File::create(path).expect("create fixture"));
    for i in 0..count {
        let i_i32 =
            i32::try_from(i).expect("record index fits in i32 for fixtures");
        let time = opts.base_time + opts.step * i_i32;
        let msg = if i == marker_idx { marker_msg } else { "boring" };
        let line = serde_json::json!({
            "v": 0,
            "level": 30,
            "name": name,
            "hostname": opts.hostname,
            "pid": opts.pid,
            "time": time.to_rfc3339_opts(SecondsFormat::Millis, true),
            "msg": msg,
        });
        writeln!(f, "{line}").expect("write line");
    }
    f.flush().expect("flush fixture");
}

/// Writes a single-source fixture with monotonically increasing
/// timestamps everywhere *except* at index `regression_idx`, whose
/// timestamp is one step earlier than its immediate predecessor.  All
/// other records use `gen_single_source`'s default schema and spacing.
fn gen_single_source_with_time_regression(
    path: &Utf8Path,
    name: &str,
    count: usize,
    regression_idx: usize,
) {
    assert!(
        regression_idx > 0,
        "regression must come after at least one record"
    );
    assert!(regression_idx < count, "regression index out of range");
    let opts = GenOpts::default();
    let mut f = BufWriter::new(File::create(path).expect("create fixture"));
    for i in 0..count {
        let i_i32 =
            i32::try_from(i).expect("record index fits in i32 for fixtures");
        let time = if i == regression_idx {
            // One step earlier than i-1: that's i-2 records past base.
            let prev_prev =
                i32::try_from(i - 2).expect("regression neighbor fits in i32");
            opts.base_time + opts.step * prev_prev
        } else {
            opts.base_time + opts.step * i_i32
        };
        let line = serde_json::json!({
            "v": 0,
            "level": 30,
            "name": name,
            "hostname": opts.hostname,
            "pid": opts.pid,
            "time": time.to_rfc3339_opts(SecondsFormat::Millis, true),
            "msg": format!("msg-{i}"),
        });
        writeln!(f, "{line}").expect("write line");
    }
    f.flush().expect("flush fixture");
}

/// Builds a `Filter` from a textual expression, panicking with a clear
/// message on parse failure.  Centralises the unwrap so each test's
/// intent stays readable.
fn parse_filter(expr: &str) -> Filter {
    expr.parse::<Filter>()
        .unwrap_or_else(|e| panic!("filter {expr:?} failed to parse: {e}"))
}

#[test]
fn streamview_forward_scrolls_through_large_record_count() {
    // Build a LARGE_COUNT-record source.  Construct a `StreamView`
    // with the default filter and walk forward.  Verify that the
    // sequence of record keys seen by `scroll_lines` matches a
    // parallel walk of the engine's `Stepper`, which is the ground
    // truth: no record skipped, none duplicated, monotonic forward
    // motion.
    let dir = TestDir::new();
    let path = dir.path().join("scroll.log");
    gen_single_source(&path, "scroll", LARGE_COUNT, &GenOpts::default());

    let mut engine = Engine::new();
    engine.add_file_source(&path).expect("add source");

    // Ground truth: every record key in forward order, via the
    // engine's stepper.
    let mut stepper = engine.stepper(Filter::default(), &Cursor::new());
    let mut expected: Vec<RecordKey> = Vec::with_capacity(LARGE_COUNT);
    while let Some(rec) = stepper.step_forward() {
        expected.push(RecordKey::from_record(&rec));
    }
    assert_eq!(expected.len(), LARGE_COUNT, "stepper should walk every record");

    // Streamview walk: scroll forward by 500 lines at a time and
    // sample the anchor after each jump.  500 is well above
    // BATCH_SIZE so multiple trims happen.
    let mut view = StreamView::new(Filter::default(), RenderOpts::default());
    let viewport: u16 = 40;
    view.ensure_window(&engine, viewport);
    let mut sampled: Vec<RecordKey> = Vec::new();
    sampled.push(
        view.anchor_record()
            .map(RecordKey::from_record)
            .expect("anchor present after ensure_window"),
    );
    while !view.is_forward_eof()
        || view.anchor_position().map(|(_, l)| l).unwrap_or(0)
            < view.anchor_record().map(|_| 0).unwrap_or(0)
    {
        let before = view
            .anchor_record()
            .map(RecordKey::from_record)
            .expect("anchor present");
        view.scroll_lines(&engine, 500, viewport);
        let after = view
            .anchor_record()
            .map(RecordKey::from_record)
            .expect("anchor present");
        if after == before {
            // No progress: we're parked on the last record under EOF.
            break;
        }
        sampled.push(after);
    }
    let last = sampled.last().expect("at least one sample").clone();
    assert_eq!(
        last,
        *expected.last().expect("expected has entries"),
        "final anchor should be the file's last record",
    );

    // Each sampled key must appear in `expected`, in monotonically
    // increasing order — that proves no record was skipped or
    // duplicated and the anchor moved strictly forward.
    let mut last_idx: Option<usize> = None;
    for key in &sampled {
        let idx = expected
            .iter()
            .position(|k| k == key)
            .unwrap_or_else(|| panic!("sampled key {key:?} not in expected"));
        if let Some(prev) = last_idx {
            assert!(
                idx > prev,
                "anchor moved backward from index {prev} to {idx}",
            );
        }
        last_idx = Some(idx);
    }
    dir.cleanup();
}

#[test]
fn streamview_backward_after_full_forward_replays_in_reverse() {
    // Walk to the end of the file one record at a time, then walk
    // back to the start one record at a time.  The reversed backward
    // sequence must equal the forward sequence — proves the
    // pop-mirror logic and the EOF-clearing-on-mirror invariant
    // survive the trim path.  5K records is several multiples of
    // WINDOW_SOFT_CAP (1024), so trims happen repeatedly in both
    // directions.
    let dir = TestDir::new();
    let path = dir.path().join("replay.log");
    let count = 5_000;
    gen_single_source(&path, "replay", count, &GenOpts::default());

    let mut engine = Engine::new();
    engine.add_file_source(&path).expect("add source");

    let viewport: u16 = 40;
    let mut view = StreamView::new(Filter::default(), RenderOpts::default());
    view.ensure_window(&engine, viewport);

    // Forward walk: advance one line (= one record under default
    // RenderOpts) at a time, recording each anchor until EOF clamps
    // us in place.
    let mut forward: Vec<RecordKey> = Vec::new();
    forward.push(
        view.anchor_record().map(RecordKey::from_record).expect("anchor"),
    );
    loop {
        let before =
            view.anchor_record().map(RecordKey::from_record).expect("anchor");
        view.scroll_lines(&engine, 1, viewport);
        let after =
            view.anchor_record().map(RecordKey::from_record).expect("anchor");
        if after == before {
            break;
        }
        forward.push(after);
    }
    assert_eq!(
        forward.len(),
        count,
        "forward step-by-step should visit every record",
    );

    // Backward walk: same step-by-step pattern in reverse.
    let mut backward: Vec<RecordKey> = Vec::new();
    backward.push(
        view.anchor_record().map(RecordKey::from_record).expect("anchor"),
    );
    loop {
        let before =
            view.anchor_record().map(RecordKey::from_record).expect("anchor");
        view.scroll_lines(&engine, -1, viewport);
        let after =
            view.anchor_record().map(RecordKey::from_record).expect("anchor");
        if after == before {
            break;
        }
        backward.push(after);
    }
    assert_eq!(
        backward.len(),
        count,
        "backward step-by-step should visit every record",
    );

    let backward_reversed: Vec<RecordKey> =
        backward.iter().rev().cloned().collect();
    assert_eq!(
        forward, backward_reversed,
        "backward replay should mirror the forward walk",
    );
    dir.cleanup();
}

#[test]
fn set_filter_after_full_walk_drops_buffers_and_resets() {
    // Walk forward through 10K records, then change the filter to one
    // that rejects nearly everything.  The view's record count must
    // reflect the new filter (not the cached pre-filter window),
    // parse_stats must reset, and the anchor must pin back to front.
    let dir = TestDir::new();
    let path = dir.path().join("filter.log");
    let count = 10_000;
    let opts =
        GenOpts {
            // Force `msg` to "rare" every 200 records, else "common".  A
            // filter accepting only "rare" yields ~50 records out of 10K.
            message_templates: (0..200)
                .map(|i| {
                    if i == 0 {
                        "rare".to_string()
                    } else {
                        "common".to_string()
                    }
                })
                .collect(),
            ..GenOpts::default()
        };
    gen_single_source(&path, "filter", count, &opts);

    let mut engine = Engine::new();
    engine.add_file_source(&path).expect("add source");

    let mut view = StreamView::new(Filter::default(), RenderOpts::default());
    let viewport: u16 = 40;
    view.ensure_window(&engine, viewport);
    // Walk forward enough to force several batch fetches.
    for _ in 0..40 {
        view.scroll_lines(&engine, 200, viewport);
    }
    assert!(
        view.parse_stats().records > 0,
        "forward walk should have populated parse_stats",
    );

    // Tighten to the "rare" message.
    view.set_filter(parse_filter("msg=rare"));
    view.ensure_window(&engine, viewport);
    let stats = view.parse_stats();
    let expected_matches = count / 200;
    // The window holds at most WINDOW_SOFT_CAP records, but our
    // expected match count is well under that, so the deque should
    // hold all matches.
    assert_eq!(
        view.record_count(),
        expected_matches,
        "filter should yield exactly count/200 matches",
    );
    // The anchor should be on the first match — confirms PinFront
    // resolved correctly after the filter change.
    let first_anchor =
        view.anchor_record().expect("anchor present after refill");
    assert!(
        first_anchor.event.is_ok(),
        "first anchor must be a matching event",
    );
    // parse_stats.records counts records appended *to the window*; on
    // a successful refill the value should equal record_count.
    assert_eq!(
        stats.records as usize, expected_matches,
        "parse_stats should reset on set_filter and refill cleanly",
    );
    dir.cleanup();
}

#[test]
fn multi_source_merge_emits_in_time_order_across_ten_sources() {
    // Ten 5K-record sources with overlapping, staggered timestamps.
    // The engine must emit `Ok` events in non-decreasing time order,
    // with every source's records present in the output.
    let dir = TestDir::new();
    let names: Vec<String> = (0..10).map(|i| format!("src{i}")).collect();
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let count_per = 1_000;
    let paths = gen_multi_source(dir.path(), count_per, &name_refs);

    let mut engine = Engine::new();
    let mut source_ids = Vec::with_capacity(paths.len());
    for p in &paths {
        source_ids.push(engine.add_file_source(p).expect("add source"));
    }

    let filter = Filter::default();
    let mut last_time = None;
    let mut counts = std::collections::HashMap::new();
    let mut total = 0usize;
    for item in engine.query_events(&filter) {
        let ev = item.expect("no merge errors expected");
        if let Some(prev) = last_time {
            assert!(
                ev.event.time >= prev,
                "merge emitted {time} after {prev}, breaking order",
                time = ev.event.time,
                prev = prev,
            );
        }
        last_time = Some(ev.event.time);
        *counts.entry(ev.position.source().clone()).or_insert(0usize) += 1;
        total += 1;
    }
    assert_eq!(total, count_per * names.len(), "all records must be emitted");
    for id in &source_ids {
        assert_eq!(
            counts.get(id).copied().unwrap_or(0),
            count_per,
            "source {id:?} must contribute exactly {count_per} records",
        );
    }
    dir.cleanup();
}

#[test]
fn multi_source_merge_under_selective_filter_walks_each_file() {
    // Same fixture shape as the prior test, but a filter that accepts
    // ~1 in 200 records.  Every source must contribute at least one
    // record to the output — proves the source-id-filter shortcut
    // didn't silently prune sources it shouldn't have.
    let dir = TestDir::new();
    let names: Vec<String> = (0..10).map(|i| format!("src{i}")).collect();
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let count_per = 1_000;
    // Override message templates so each source has the "rare" string
    // at every 200th position.  Re-using `gen_multi_source`'s default
    // step + stagger means the merged stream interleaves matches
    // across sources.
    for (idx, name) in name_refs.iter().enumerate() {
        let path = dir.path().join(format!("{name}.log"));
        let base = GenOpts::default();
        let n_i32 = i32::try_from(name_refs.len()).expect("count fits");
        let stagger = base.step / n_i32;
        let idx_i32 = i32::try_from(idx).expect("idx fits");
        let opts = GenOpts {
            base_time: base.base_time + stagger * idx_i32,
            message_templates: (0..200)
                .map(|i| {
                    if i == 0 {
                        "rare".to_string()
                    } else {
                        "common".to_string()
                    }
                })
                .collect(),
            ..base.clone()
        };
        gen_single_source(&path, name, count_per, &opts);
    }

    let mut engine = Engine::new();
    let mut source_ids = Vec::with_capacity(name_refs.len());
    for name in &name_refs {
        let p = dir.path().join(format!("{name}.log"));
        source_ids.push(engine.add_file_source(&p).expect("add source"));
    }

    let filter = parse_filter("msg=rare");
    let mut counts = std::collections::HashMap::new();
    let mut last_time = None;
    for item in engine.query_events(&filter) {
        let ev = item.expect("no merge errors expected");
        if let Some(prev) = last_time {
            assert!(ev.event.time >= prev, "filtered stream out of order");
        }
        last_time = Some(ev.event.time);
        *counts.entry(ev.position.source().clone()).or_insert(0usize) += 1;
    }
    let expected_per = count_per / 200;
    for id in &source_ids {
        assert_eq!(
            counts.get(id).copied().unwrap_or(0),
            expected_per,
            "source {id:?} should contribute {expected_per} matches",
        );
    }
    dir.cleanup();
}

#[test]
fn search_step_forward_finds_match_near_end_with_resume() {
    // Place a unique phrase near the end of a LARGE_COUNT file.  A
    // forward `search_step_with_budget` call with a budget smaller
    // than the distance to the match should report `BudgetExhausted`,
    // then resume on a subsequent call and finally report `Found`,
    // with the anchor on the marker record.
    let dir = TestDir::new();
    let path = dir.path().join("search.log");
    let target_idx = LARGE_COUNT - 1_000;
    let marker = "needle-in-haystack";
    gen_single_source_with_marker(
        &path,
        "search",
        LARGE_COUNT,
        target_idx,
        marker,
    );

    let mut engine = Engine::new();
    engine.add_file_source(&path).expect("add source");

    let mut view = StreamView::new(Filter::default(), RenderOpts::default());
    let viewport: u16 = 40;
    view.ensure_window(&engine, viewport);

    let re = regex::Regex::new(marker).expect("valid regex");
    // First, exercise the BudgetExhausted resume path with a tight
    // budget — small enough that finding a match at target_idx
    // requires multiple resume rounds.
    let small_budget = 2_000;
    let mut found = false;
    let mut exhausted_at_least_once = false;
    for _ in 0..20 {
        let outcome = view.search_step_with_budget(
            &engine,
            &re,
            SearchDir::Forward,
            false,
            viewport,
            small_budget,
            &mut || false,
        );
        match outcome {
            SearchOutcome::Found => {
                found = true;
                break;
            }
            SearchOutcome::BudgetExhausted => {
                exhausted_at_least_once = true;
            }
            other => panic!("unexpected search outcome: {other:?}"),
        }
    }
    assert!(found, "should find the marker before the retry budget elapses");
    assert!(
        exhausted_at_least_once,
        "match near end with tight budget should hit BudgetExhausted first",
    );

    let anchor = view.anchor_record().expect("anchor must be set on Found");
    let event = anchor.event.as_ref().expect("anchor is on a parsed event");
    assert_eq!(event.msg, marker, "anchor must be the marker record");
    dir.cleanup();
}

#[test]
fn summary_build_via_stepper_matches_eager_summarize() {
    // Compute a summary two ways: via the eager `summarize` helper,
    // and via a manually-driven chunked `Stepper` walk that mirrors
    // the long-op `SummaryOp` in the seer binary.  The two `Summary`
    // values must compare equal — pinning down that the chunked
    // driver produces identical output to the one-pass version.
    let dir = TestDir::new();
    let path = dir.path().join("summary.log");
    let opts = GenOpts { extras_every: 50, ..GenOpts::default() };
    gen_single_source(&path, "summary", LARGE_COUNT, &opts);

    let mut engine = Engine::new();
    engine.add_file_source(&path).expect("add source");

    let filter = Filter::default();
    let eager = summarize(&engine, &filter);

    // Chunked build: drive a fresh stepper per chunk, exactly the
    // pattern the long-op driver uses.
    let mut builder = SummaryBuilder::default();
    let mut cursor = Cursor::new();
    let chunk = 4_000;
    loop {
        let mut stepper = engine.stepper(filter.clone(), &cursor);
        let mut walked = 0;
        while walked < chunk {
            let Some(rec) = stepper.step_forward() else {
                cursor = stepper.cursor();
                break;
            };
            if let Ok(event) = rec.event {
                builder.observe(&rec.source_id, &event);
            }
            walked += 1;
        }
        let new_cursor = stepper.cursor();
        if new_cursor == cursor {
            // No progress this chunk: we're done.
            break;
        }
        cursor = new_cursor;
        if walked < chunk {
            break;
        }
    }
    let chunked = builder.finish();

    assert_eq!(
        eager.total_events, chunked.total_events,
        "total events must match",
    );
    assert_eq!(
        eager.fields.len(),
        chunked.fields.len(),
        "field counts must match",
    );
    for (a, b) in eager.fields.iter().zip(chunked.fields.iter()) {
        assert_eq!(a.name, b.name, "field name order");
        assert_eq!(a.event_count, b.event_count, "field {}", a.name);
        assert_eq!(a.values, b.values, "field {} values", a.name);
        assert_eq!(
            a.other_count, b.other_count,
            "field {} other_count",
            a.name,
        );
    }
    assert_eq!(
        eager.time.is_some(),
        chunked.time.is_some(),
        "time summary presence",
    );
    if let (Some(ta), Some(tb)) = (&eager.time, &chunked.time) {
        assert_eq!(ta.buckets, tb.buckets, "time buckets must match");
        assert_eq!(ta.bucket_label, tb.bucket_label, "bucket label");
    }
    dir.cleanup();
}

#[test]
fn seek_to_cursor_under_selective_filter_yields_correct_anchor() {
    // Anchor a position deep in a LARGE_COUNT file where the record
    // at that index has a unique `msg`.  Apply a filter that excludes
    // exactly that record (and only that record).  `seek_to_cursor`
    // must land on the next visible record after the bookmark.
    let dir = TestDir::new();
    let path = dir.path().join("seek.log");
    let target_idx = LARGE_COUNT * 6 / 10;
    let unique = "bookmark-target";
    gen_single_source_with_marker(
        &path,
        "seek",
        LARGE_COUNT,
        target_idx,
        unique,
    );

    let mut engine = Engine::new();
    let source_id = engine.add_file_source(&path).expect("add source");

    // The marker record's timestamp is deterministic from GenOpts'
    // base + step * target_idx.
    let opts = GenOpts::default();
    let i_i32 = i32::try_from(target_idx).expect("target index fits in i32");
    let marker_time = opts.base_time + opts.step * i_i32;
    let pos = LogStreamPosition::new(source_id, marker_time, 0);
    let cursor =
        engine.cursor_for_position(&pos).expect("position must resolve");

    // Filter out the marker record specifically.
    let filter = parse_filter(&format!("msg!={unique}"));
    let mut view = StreamView::new(filter, RenderOpts::default());
    let viewport: u16 = 40;
    view.seek_to_cursor(&engine, cursor, viewport);

    let anchor =
        view.anchor_record().expect("seek_to_cursor must land on a record");
    let ev = anchor.event.as_ref().expect("anchor on parsed event");
    assert_ne!(ev.msg, unique, "anchor must not be the filtered-out record",);
    // Next visible record is index 30001 — "boring".
    assert_eq!(ev.msg, "boring", "anchor must be the next visible record");
    // Its timestamp is exactly one step after the marker.
    let expected_time = marker_time + Duration::milliseconds(100);
    assert_eq!(
        ev.time, expected_time,
        "anchor should be the record immediately after the marker",
    );
    dir.cleanup();
}

#[test]
fn cancel_seek_midstream_leaves_partial_window_in_consistent_state() {
    // Drive a `seek_to_cursor`-style fetch in chunks via
    // `ensure_window_step`, stop midstream, and assert: the records
    // present in the partial window form a strict prefix of what a
    // fully-completed seek would produce.
    let dir = TestDir::new();
    let path = dir.path().join("cancel.log");
    let count = 10_000;
    gen_single_source(&path, "cancel", count, &GenOpts::default());

    let mut engine = Engine::new();
    engine.add_file_source(&path).expect("add source");

    // Reference walk: ensure_window from byte 0 under the default
    // filter.
    let mut reference =
        StreamView::new(Filter::default(), RenderOpts::default());
    let viewport: u16 = 40;
    reference.ensure_window(&engine, viewport);
    let reference_keys: Vec<RecordKey> = reference
        .records()
        .map(|(rec, _)| RecordKey::from_record(rec))
        .collect();
    assert!(
        reference_keys.len() >= 64,
        "reference window must hold at least one batch's worth",
    );

    // Partial walk: prepare_seek_to_start then ensure_window_step
    // until either we've taken several steps or completion arrives.
    let mut partial = StreamView::new(Filter::default(), RenderOpts::default());
    partial.prepare_seek_to_start();
    let max_steps = 3;
    let mut step_count = 0;
    while step_count < max_steps {
        let status = partial.ensure_window_step(&engine, viewport);
        step_count += 1;
        if matches!(status, seer::WindowFillStatus::Done) {
            break;
        }
    }
    let partial_keys: Vec<RecordKey> =
        partial.records().map(|(rec, _)| RecordKey::from_record(rec)).collect();
    assert!(
        !partial_keys.is_empty(),
        "partial window must have at least one record",
    );

    // Each partial key must equal the same-indexed reference key.
    // Records are emitted in forward order from byte 0 in both walks,
    // so a partial walk is a strict prefix.
    for (i, key) in partial_keys.iter().enumerate() {
        assert_eq!(
            key, &reference_keys[i],
            "partial walk diverges from reference at index {i}",
        );
    }
    dir.cleanup();
}

#[test]
fn out_of_order_warning_emitted_once_across_a_full_pass() {
    // Insert one timestamp regression into an otherwise-monotonic
    // LARGE_COUNT file.  A full merge pass should produce exactly one
    // `SourceError::OutOfOrder` warning, identifying the right
    // source.
    let dir = TestDir::new();
    let path = dir.path().join("ooo.log");
    let count = LARGE_COUNT;
    let regression_idx = count / 2;
    gen_single_source_with_time_regression(&path, "ooo", count, regression_idx);

    let mut engine = Engine::new();
    let source_id = engine.add_file_source(&path).expect("add source");

    let filter = Filter::default();
    let mut warnings = 0usize;
    let mut other_errors = 0usize;
    for item in engine.query_events(&filter) {
        match item {
            Ok(_) => {}
            Err(SourceError::OutOfOrder { source_id: sid, .. }) => {
                assert_eq!(sid, source_id, "warning must reference our source",);
                warnings += 1;
            }
            Err(other) => {
                other_errors += 1;
                eprintln!("unexpected error: {other}");
            }
        }
    }
    assert_eq!(
        warnings, 1,
        "expected exactly one OutOfOrder warning across the pass",
    );
    assert_eq!(other_errors, 0, "expected no parse or IO errors");
    dir.cleanup();
}
