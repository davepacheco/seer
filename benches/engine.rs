// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Criterion benchmarks for the seer engine and streamview hot paths.
//!
//! Each bench builds its fixture once outside `b.iter` so the measurement
//! reflects only the operation under test, not fixture generation.
//!
//! Sample size is reduced to 10 (down from criterion's default 100)
//! because most of these benches drain a 50K-record file each
//! iteration; 100 samples would make a single bench run for several
//! minutes.

use camino_tempfile::Utf8TempDir;
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use seer::test_fixtures::{GenOpts, gen_single_source};
use seer::{
    ByteOffset, Cursor, Direction, Engine, FileSource, Filter, RenderOpts,
    Source, StreamView, summarize,
};
use std::hint::black_box;
use std::time::Duration;

/// Records per single-source fixture.  Big enough that a full
/// forward/backward scan exercises the storage layer's chunked I/O
/// repeatedly and produces a number criterion can compare meaningfully
/// across runs.
const BENCH_COUNT: usize = 50_000;

/// Per-source count for the multi-source merge bench.  Ten of these
/// gives a 50K total, matching the single-source fixtures' parse cost
/// but routed through the k-way merge.
const MULTI_PER_SOURCE: usize = 5_000;

/// Number of sources in the multi-source merge bench.
const MULTI_SOURCE_COUNT: usize = 10;

/// `GenOpts` whose message templates accept exactly 1 in 100 records
/// under the filter `msg=rare`.  Used for the selective-filter source
/// query bench.
fn selective_opts() -> GenOpts {
    let templates: Vec<String> = (0..100)
        .map(|i| if i == 0 { "rare".to_string() } else { "common".to_string() })
        .collect();
    GenOpts { message_templates: templates, ..GenOpts::default() }
}

/// Builds a temp dir + 50K-record single-source fixture and returns
/// both the dir (kept alive for cleanup) and the opened `FileSource`.
fn build_single_source(opts: &GenOpts) -> (Utf8TempDir, FileSource) {
    let dir = Utf8TempDir::new().expect("temp dir");
    let path = dir.path().join("bench.log");
    gen_single_source(&path, "bench", BENCH_COUNT, opts);
    let source = FileSource::open(&path).expect("open source");
    (dir, source)
}

/// Builds a temp dir + multi-source engine with `MULTI_SOURCE_COUNT`
/// staggered sources and returns both.
fn build_multi_source_engine() -> (Utf8TempDir, Engine) {
    let dir = Utf8TempDir::new().expect("temp dir");
    let mut engine = Engine::new();
    let base = GenOpts::default();
    let n_i32 = i32::try_from(MULTI_SOURCE_COUNT).expect("count fits");
    let stagger = base.step / n_i32;
    for idx in 0..MULTI_SOURCE_COUNT {
        let name = format!("src{idx}");
        let path = dir.path().join(format!("{name}.log"));
        let idx_i32 = i32::try_from(idx).expect("idx fits");
        let opts = GenOpts {
            base_time: base.base_time + stagger * idx_i32,
            ..base.clone()
        };
        gen_single_source(&path, &name, MULTI_PER_SOURCE, &opts);
        engine.add_file_source(&path).expect("add source");
    }
    (dir, engine)
}

fn bench_source_query_forward_unfiltered(c: &mut Criterion) {
    let (_dir, source) = build_single_source(&GenOpts::default());
    let filter = Filter::default();
    c.bench_function("source_query_forward_unfiltered", |b| {
        b.iter(|| {
            let batch = source
                .query_bounded(
                    ByteOffset::ZERO,
                    Direction::Forward,
                    BENCH_COUNT,
                    None,
                    &filter,
                )
                .expect("query");
            black_box(batch);
        });
    });
}

fn bench_source_query_forward_selective(c: &mut Criterion) {
    let (_dir, source) = build_single_source(&selective_opts());
    let filter = "msg=rare".parse::<Filter>().expect("filter");
    c.bench_function("source_query_forward_selective_1_in_100", |b| {
        b.iter(|| {
            let batch = source
                .query_bounded(
                    ByteOffset::ZERO,
                    Direction::Forward,
                    BENCH_COUNT,
                    None,
                    &filter,
                )
                .expect("query");
            black_box(batch);
        });
    });
}

fn bench_source_query_backward_unfiltered(c: &mut Criterion) {
    let (_dir, source) = build_single_source(&GenOpts::default());
    let filter = Filter::default();
    let end = source.byte_len().expect("byte len");
    c.bench_function("source_query_backward_unfiltered", |b| {
        b.iter(|| {
            let batch = source
                .query_bounded(
                    ByteOffset::from(end),
                    Direction::Backward,
                    BENCH_COUNT,
                    None,
                    &filter,
                )
                .expect("query");
            black_box(batch);
        });
    });
}

fn bench_stepper_forward_drains(c: &mut Criterion) {
    let (_dir, source) = build_single_source(&GenOpts::default());
    let mut engine = Engine::new();
    engine.add_file_source(source.path()).expect("add source");
    let filter = Filter::default();
    c.bench_function("stepper_forward_drains_50k_unfiltered", |b| {
        b.iter(|| {
            let mut stepper = engine.stepper(filter.clone(), &Cursor::new());
            while let Some(rec) = stepper.step_forward() {
                black_box(rec);
            }
        });
    });
}

fn bench_stepper_forward_multi_source(c: &mut Criterion) {
    let (_dir, engine) = build_multi_source_engine();
    let filter = Filter::default();
    c.bench_function("stepper_forward_drains_10x5k_multi_source", |b| {
        b.iter(|| {
            let mut stepper = engine.stepper(filter.clone(), &Cursor::new());
            while let Some(rec) = stepper.step_forward() {
                black_box(rec);
            }
        });
    });
}

fn bench_stepper_forward_then_backward(c: &mut Criterion) {
    // Microbenchmark for the lookbehind cache: with a stepper already
    // positioned in the middle of the file (so both buffers carry
    // content), step_forward followed by step_backward should hit the
    // cache without re-reading.
    let (_dir, source) = build_single_source(&GenOpts::default());
    let mut engine = Engine::new();
    engine.add_file_source(source.path()).expect("add source");
    let filter = Filter::default();
    c.bench_function("stepper_step_forward_then_backward_one_each", |b| {
        b.iter_batched(
            || {
                // Setup: build a stepper and prime it by stepping
                // forward 64 records (one batch) so backward has
                // something to mirror from.
                let mut stepper =
                    engine.stepper(filter.clone(), &Cursor::new());
                for _ in 0..64 {
                    stepper.step_forward();
                }
                stepper
            },
            |mut stepper| {
                let f = stepper.step_forward();
                let b = stepper.step_backward();
                black_box((f, b));
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_stepper_set_filter_then_step(c: &mut Criterion) {
    // Full filter rebuild on a populated stepper, then walk forward
    // by 1000 records.  Exercises the buffer-clear + fresh-fill path
    // that runs on every interactive filter change.
    let (_dir, source) = build_single_source(&selective_opts());
    let mut engine = Engine::new();
    engine.add_file_source(source.path()).expect("add source");
    let new_filter = "msg=rare".parse::<Filter>().expect("filter");
    c.bench_function("stepper_set_filter_then_step_forward", |b| {
        b.iter_batched(
            || {
                // Setup: populate the stepper under the default
                // filter so set_filter has to clear meaningful state.
                let mut stepper =
                    engine.stepper(Filter::default(), &Cursor::new());
                for _ in 0..500 {
                    stepper.step_forward();
                }
                stepper
            },
            |mut stepper| {
                stepper.set_filter(new_filter.clone());
                let mut count = 0;
                while count < 1_000 {
                    if stepper.step_forward().is_none() {
                        break;
                    }
                    count += 1;
                }
                black_box(count);
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_streamview_ensure_window(c: &mut Criterion) {
    let (_dir, source) = build_single_source(&GenOpts::default());
    let mut engine = Engine::new();
    engine.add_file_source(source.path()).expect("add source");
    c.bench_function("streamview_ensure_window_default_filter", |b| {
        b.iter(|| {
            let mut view =
                StreamView::new(Filter::default(), RenderOpts::default());
            view.ensure_window(&engine, 80);
            black_box(view.record_count());
        });
    });
}

fn bench_streamview_scroll_past_edge(c: &mut Criterion) {
    // Scroll the viewport 200 lines forward against a 1024-record
    // window built from a 50K file — every iteration crosses the
    // window edge and exercises the slide+trim path.
    let (_dir, source) = build_single_source(&GenOpts::default());
    let mut engine = Engine::new();
    engine.add_file_source(source.path()).expect("add source");
    c.bench_function("streamview_scroll_lines_past_window_edge", |b| {
        b.iter_batched(
            || {
                let mut view =
                    StreamView::new(Filter::default(), RenderOpts::default());
                view.ensure_window(&engine, 80);
                view
            },
            |mut view| {
                view.scroll_lines(&engine, 200, 80);
                black_box(view.record_count());
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_summarize(c: &mut Criterion) {
    let (_dir, source) = build_single_source(&GenOpts {
        extras_every: 50,
        ..GenOpts::default()
    });
    let mut engine = Engine::new();
    engine.add_file_source(source.path()).expect("add source");
    let filter = Filter::default();
    c.bench_function("summarize_50k_unfiltered", |b| {
        b.iter(|| {
            let summary = summarize(&engine, &filter);
            black_box(summary);
        });
    });
}

/// Mimics `SummaryOp` in the TUI: walks the engine via `Stepper`
/// (not `EventStream`, which is what `summarize()` uses), folding each
/// record into a `SummaryBuilder`.  Rebuilds the stepper from the
/// saved cursor every `LONG_OP_CHUNK_RECORDS` records to exercise the
/// long-op driver's per-chunk reconstruction cost.
fn bench_summarize_via_stepper(c: &mut Criterion) {
    use seer::SummaryBuilder;
    const CHUNK: usize = 4_000;
    let (_dir, source) = build_single_source(&GenOpts {
        extras_every: 50,
        ..GenOpts::default()
    });
    let mut engine = Engine::new();
    engine.add_file_source(source.path()).expect("add source");
    let filter = Filter::default();
    c.bench_function("summarize_via_stepper_50k_unfiltered", |b| {
        b.iter(|| {
            let mut builder = SummaryBuilder::default();
            let mut cursor = Cursor::new();
            'outer: loop {
                let mut stepper = engine.stepper(filter.clone(), &cursor);
                let mut count = 0;
                while count < CHUNK {
                    let Some(rec) = stepper.step_forward() else {
                        builder.finish();
                        break 'outer;
                    };
                    if let Ok(event) = rec.event {
                        builder.observe(&rec.source_id, &event);
                    }
                    count += 1;
                }
                cursor = stepper.cursor();
            }
        });
    });
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    targets =
        bench_source_query_forward_unfiltered,
        bench_source_query_forward_selective,
        bench_source_query_backward_unfiltered,
        bench_stepper_forward_drains,
        bench_stepper_forward_multi_source,
        bench_stepper_forward_then_backward,
        bench_stepper_set_filter_then_step,
        bench_streamview_ensure_window,
        bench_streamview_scroll_past_edge,
        bench_summarize,
        bench_summarize_via_stepper,
);
criterion_main!(benches);
