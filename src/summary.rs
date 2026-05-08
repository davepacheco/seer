// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Field and time histograms over an [`Engine`]'s events.
//!
//! A [`Summary`] is the data behind a Summary tab: for the events that
//! pass the active filter, it captures
//!
//! - the top 10 most-frequent top-level JSON field names (per source,
//!   then unioned across sources, as the spec asks for) and, for each,
//!   the most common values that field takes;
//! - a histogram of event counts in time buckets sized so that the full
//!   range is divided into roughly 30 buckets (1m / 1h / 1d).
//!
//! The Summary is computed in a single pass: every event contributes to
//! every field's value count and to one time bucket; afterwards the
//! per-source top-10 lists pick which fields survive and in what order
//! they're displayed.  Keeping all per-field value counts during the
//! pass costs more memory than a two-pass design that learns the top-10
//! first and only keeps values for those fields, but we already pay the
//! cost of walking the events once on the way to the histogram and a
//! single hash map per field is small in practice.
//!
//! The `time` field is handled specially: it never appears in the field
//! list (it would dominate it with one bucket per RFC3339 timestamp).
//! Its place is taken by the time-bucket histogram.
//!
//! The summary is purely a data type — formatting it into display lines
//! lives in [`format_summary`], which turns it into the same
//! `Vec<String>` shape that the regular log view uses so the TUI's
//! viewport, search, and rendering paths can be reused unchanged.

use crate::engine::Engine;
use crate::event::Event;
use crate::filter::Filter;
use crate::source::SourceId;
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeSet, HashMap};

/// How many fields we keep per source before unioning across sources.
const TOP_FIELDS_PER_SOURCE: usize = 10;

/// How many distinct values per field are listed in the histogram.  Any
/// further values are summarized as "(N more)" — the user can drill
/// down by adding a filter predicate, so the cap keeps the histogram
/// readable on tall screens without throwing data away invisibly.
pub const TOP_VALUES_PER_FIELD: usize = 20;

/// Width (in cells) of the histogram bars in [`format_summary`].
const HISTOGRAM_BAR_WIDTH: usize = 40;

/// Aggregate of every event matching a filter, ready for histogram
/// rendering.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// Total number of events the summary was built from.
    pub total_events: u64,
    /// Field histograms, ordered by total occurrences (descending).
    /// Length is at most `TOP_FIELDS_PER_SOURCE * num_sources` but
    /// typically much less because top-10 lists overlap heavily.
    pub fields: Vec<FieldSummary>,
    /// Time-bucket histogram.  `None` when no events were observed.
    pub time: Option<TimeSummary>,
}

/// Histogram of values for a single top-level field.
#[derive(Debug, Clone)]
pub struct FieldSummary {
    /// Top-level JSON key.
    pub name: String,
    /// Number of events that included this field.  Equal to
    /// `values.iter().map(|(_, c)| c).sum::<u64>() + other_count`.
    pub event_count: u64,
    /// Top values by frequency (desc), then by value text (asc) to make
    /// the order deterministic when counts tie.  Each value is rendered
    /// in its canonical JSON form (so a string is `"foo"`, a number is
    /// `42`, etc.) — the same shape extras get in [`crate::render`].
    pub values: Vec<(String, u64)>,
    /// Number of events whose value didn't fit in `values` (i.e., was
    /// less frequent than the cutoff).  Surfaced so the histogram can
    /// say "(plus 17 less common values)" rather than silently truncating.
    pub other_count: u64,
    /// Number of distinct values omitted from `values` (corresponds to
    /// `other_count` aggregated across distinct values).  Lets the
    /// renderer say "(plus 17 less common values, totalling N events)"
    /// without recomputing the underlying set.
    pub other_distinct: u64,
    /// Number of events that did not include this field at all.  Equal
    /// to `summary.total_events - event_count`; cached so the renderer
    /// can show "(no value)" as its own histogram row without needing
    /// to know the parent total.
    pub no_value_count: u64,
}

/// Histogram of event counts over time.
#[derive(Debug, Clone)]
pub struct TimeSummary {
    /// Short label for the bucket size shown in the histogram header
    /// (`"1m"`, `"1h"`, `"1d"`).
    pub bucket_label: &'static str,
    /// Width of one bucket.
    pub bucket_duration: Duration,
    /// Buckets ordered by start time, ascending.  Empty buckets in the
    /// middle of the range are present with `count = 0` so the
    /// histogram shows quiet periods rather than silently compressing
    /// them out.
    pub buckets: Vec<(DateTime<Utc>, u64)>,
}

/// Returns a [`Summary`] of every event in `engine` that passes
/// `filter`.
///
/// Traverses the event stream once.  Source-id filtering is applied at
/// the engine level (so excluded sources are never opened); event-level
/// predicates are applied per record.  Parse errors and out-of-order
/// warnings are skipped — the summary describes what was successfully
/// parsed and accepted by the filter.
pub fn summarize(engine: &Engine, filter: &Filter) -> Summary {
    let mut builder = SummaryBuilder::default();
    // `flatten` discards `Err` items (parse errors and out-of-order
    // warnings): the summary describes only what was successfully
    // parsed.
    for ee in engine.query_events(filter).flatten() {
        builder.observe(ee.position.source(), &ee.event);
    }
    builder.finish()
}

/// Streaming accumulator for a [`Summary`].
///
/// Public so callers that want to share a single pass over the engine
/// (e.g. the TUI, which also wants the [`crate::engine::EventStream`]'s
/// parse-rate counters) can drive the same accumulator the convenience
/// [`summarize`] function uses.  Keeps everything in memory until
/// [`Self::finish`] trims to the top-K shapes.
#[derive(Default)]
pub struct SummaryBuilder {
    total_events: u64,
    /// Per-source field counts so we can compute top-K per source.
    /// Stored eagerly because we don't know which fields will survive
    /// the per-source top-K cut until the pass is complete.
    per_source_field_counts: HashMap<SourceId, HashMap<String, u64>>,
    /// Per-field, per-value count.  Populated for every field
    /// encountered, regardless of whether the field will eventually
    /// make the top-K — pruning happens at finish time.
    field_value_counts: HashMap<String, HashMap<String, u64>>,
    /// Per-field total occurrence count, summed across all sources.
    /// Used to order the surviving fields in the final summary.
    field_total_counts: HashMap<String, u64>,
    /// Earliest and latest event timestamp observed.  Used to size the
    /// time-bucket histogram.
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// Running per-bucket-start counts; bucket size isn't known until
    /// we see the full range, so we accumulate raw timestamps and
    /// rebucket at finish time.
    times: Vec<DateTime<Utc>>,
}

impl SummaryBuilder {
    /// Folds one event into the accumulator.  Caller is responsible for
    /// deciding whether the event passed the filter.
    pub fn observe(&mut self, source: &SourceId, event: &Event) {
        self.total_events += 1;
        // Update time bookkeeping first so we can drop the timestamp
        // before iterating fields (since `time` is excluded from the
        // field list).
        self.times.push(event.time);
        match self.time_range {
            None => self.time_range = Some((event.time, event.time)),
            Some((min, max)) => {
                self.time_range = Some((min.min(event.time), max.max(event.time)));
            }
        }
        let per_source =
            self.per_source_field_counts.entry(source.clone()).or_default();
        for (name, value) in iter_fields(event) {
            // Time is recorded separately above; skip it here so it
            // doesn't crowd out other fields in the per-source top-K.
            if name == "time" {
                continue;
            }
            *per_source.entry(name.clone()).or_default() += 1;
            *self.field_total_counts.entry(name.clone()).or_default() += 1;
            let key = canonical_value(&value);
            *self
                .field_value_counts
                .entry(name)
                .or_default()
                .entry(key)
                .or_default() += 1;
        }
    }

    /// Consumes the builder and returns the finished [`Summary`].
    pub fn finish(self) -> Summary {
        // Take top-K per source, union the surviving names.  BTreeSet
        // for deterministic iteration order before we sort by count.
        let mut surviving: BTreeSet<String> = BTreeSet::new();
        for counts in self.per_source_field_counts.values() {
            let mut by_count: Vec<(&String, &u64)> = counts.iter().collect();
            // Descending by count, then ascending by name for tie-break.
            by_count.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            for (name, _) in by_count.into_iter().take(TOP_FIELDS_PER_SOURCE) {
                surviving.insert(name.clone());
            }
        }
        // Order the surviving fields by total count desc, name asc.
        let mut surviving: Vec<String> = surviving.into_iter().collect();
        surviving.sort_by(|a, b| {
            self.field_total_counts
                .get(b)
                .unwrap_or(&0)
                .cmp(self.field_total_counts.get(a).unwrap_or(&0))
                .then(a.cmp(b))
        });
        let total_events = self.total_events;
        let fields = surviving
            .into_iter()
            .map(|name| {
                let value_counts =
                    self.field_value_counts.get(&name).cloned().unwrap_or_default();
                build_field_summary(name, value_counts, total_events)
            })
            .collect();

        let time = self.time_range.map(|(min, max)| {
            let (bucket_duration, bucket_label) = pick_bucket_size(max - min);
            build_time_summary(
                bucket_duration,
                bucket_label,
                min,
                max,
                &self.times,
            )
        });

        Summary { total_events: self.total_events, fields, time }
    }
}

fn build_field_summary(
    name: String,
    value_counts: HashMap<String, u64>,
    total_events: u64,
) -> FieldSummary {
    let event_count: u64 = value_counts.values().sum();
    let mut all: Vec<(String, u64)> = value_counts.into_iter().collect();
    // Descending by count, then ascending by value for stable display.
    all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let (top, rest) = if all.len() > TOP_VALUES_PER_FIELD {
        let rest = all.split_off(TOP_VALUES_PER_FIELD);
        (all, rest)
    } else {
        (all, Vec::new())
    };
    let other_count: u64 = rest.iter().map(|(_, c)| c).sum();
    let other_distinct = rest.len() as u64;
    let no_value_count = total_events.saturating_sub(event_count);
    FieldSummary {
        name,
        event_count,
        values: top,
        other_count,
        other_distinct,
        no_value_count,
    }
}

fn build_time_summary(
    bucket_duration: Duration,
    bucket_label: &'static str,
    min: DateTime<Utc>,
    max: DateTime<Utc>,
    times: &[DateTime<Utc>],
) -> TimeSummary {
    let bucket_secs = bucket_duration.num_seconds().max(1);
    // Align min and max to bucket boundaries.  The number of buckets is
    // (end-start)/unit + 1 so the bucket containing `max` itself is
    // included; this is what makes "events at minute 0 and minute 5"
    // produce six buckets, not five.
    let start_secs = floor_to_bucket(min.timestamp(), bucket_secs);
    let end_secs = floor_to_bucket(max.timestamp(), bucket_secs);
    let n = ((end_secs - start_secs) / bucket_secs) as usize + 1;
    let mut counts = vec![0u64; n];
    for t in times {
        let secs = floor_to_bucket(t.timestamp(), bucket_secs);
        let idx = ((secs - start_secs) / bucket_secs) as usize;
        // Guard against an out-of-range index from a future caller that
        // passes a `times` slice not bounded by `min`/`max`.  In the
        // current code path the arithmetic above is always in range.
        if idx < counts.len() {
            counts[idx] += 1;
        }
    }
    let buckets = (0..n)
        .map(|i| {
            let bucket_start_secs = start_secs + (i as i64) * bucket_secs;
            let bucket_start = DateTime::from_timestamp(bucket_start_secs, 0)
                .expect("aligned timestamp");
            (bucket_start, counts[i])
        })
        .collect();
    TimeSummary { bucket_label, bucket_duration, buckets }
}

/// Floors `secs` to the nearest multiple of `bucket_secs` at or below.
/// Handles negative timestamps correctly (stick to floor, not truncation
/// toward zero) so a UTC timestamp before 1970 — vanishingly unlikely in
/// our domain, but cheap to get right — still falls into a stable
/// bucket.
fn floor_to_bucket(secs: i64, bucket_secs: i64) -> i64 {
    let r = secs.rem_euclid(bucket_secs);
    secs - r
}

/// Choose a bucket size that yields about 30 buckets over `range`.
///
/// Candidates are 1m, 1h, 1d (the spec calls these out explicitly).
/// We pick the unit that produces a bucket count closest to 30; ties
/// prefer the smaller unit so a flat range never collapses into one
/// huge bucket.
fn pick_bucket_size(range: Duration) -> (Duration, &'static str) {
    let candidates: [(Duration, &'static str); 3] = [
        (Duration::minutes(1), "1m"),
        (Duration::hours(1), "1h"),
        (Duration::days(1), "1d"),
    ];
    let target = 30i64;
    let range_secs = range.num_seconds().max(0);
    let mut best = candidates[0];
    let mut best_dist = i64::MAX;
    for cand in candidates {
        let unit_secs = cand.0.num_seconds().max(1);
        // At least one bucket: a zero-or-negative range still produces
        // a single bucket containing the lone observed timestamp.
        let n = (range_secs / unit_secs).max(1);
        let dist = (n - target).abs();
        if dist < best_dist {
            best = cand;
            best_dist = dist;
        }
    }
    best
}

/// JSON-string representation of `value` used as the histogram key and
/// label.  String values keep their quotes (so `"Nexus"` and the bare
/// identifier `Nexus` would not collide), matching the canonical form
/// [`crate::render::format_event`] uses for extras.
fn canonical_value(value: &serde_json::Value) -> String {
    value.to_string()
}

/// Top-level JSON fields of `event` as `(name, value)` pairs.  Core
/// fields first (in bunyan layout order), then extras in BTreeMap
/// order.  `time` is included; the summary builder strips it before
/// counting because it gets the time-bucket histogram instead.
fn iter_fields(event: &Event) -> Vec<(String, serde_json::Value)> {
    use serde_json::json;
    let mut out: Vec<(String, serde_json::Value)> =
        Vec::with_capacity(7 + event.extra.len());
    out.push((
        "time".to_string(),
        serde_json::Value::String(event.time.to_rfc3339()),
    ));
    out.push(("level".to_string(), json!(event.level.as_bunyan_number())));
    out.push((
        "name".to_string(),
        serde_json::Value::String(event.name.to_string()),
    ));
    out.push((
        "hostname".to_string(),
        serde_json::Value::String(event.hostname.to_string()),
    ));
    // pid is a u32 wrapped in a newtype with `Display` only; re-parse
    // its decimal form back into a JSON number so the histogram label
    // reads `42` (a number) rather than `"42"` (a quoted string).  A
    // failed parse falls back to the string form so we never lose the
    // value.
    let pid_str = event.pid.to_string();
    let pid_value = pid_str
        .parse::<u64>()
        .map(serde_json::Value::from)
        .unwrap_or(serde_json::Value::String(pid_str));
    out.push(("pid".to_string(), pid_value));
    out.push(("msg".to_string(), serde_json::Value::String(event.msg.clone())));
    out.push(("v".to_string(), json!(event.v)));
    for (k, v) in &event.extra {
        out.push((k.clone(), v.clone()));
    }
    out
}

/// Label used for the histogram entry counting events that did not
/// include the field at all.  Picked to be visually distinct from any
/// JSON-canonical value: a plain JSON string would be `"..."` (with
/// quotes), a number is bare digits, etc.
const NO_VALUE_LABEL: &str = "(no value)";

/// Renders a [`Summary`] into display lines (one per row).  The shape
/// mirrors the regular log view's `Vec<String>` return so the TUI's
/// viewport, search, and styling code can render Summary tabs without
/// branching.
pub fn format_summary(summary: &Summary) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!(
        "Summary: {} event{}",
        summary.total_events,
        if summary.total_events == 1 { "" } else { "s" },
    ));
    if summary.total_events == 0 {
        out.push(String::new());
        out.push("(no events match the active filter)".to_string());
        return out;
    }

    // Pre-compute each field's display rows (with `(no value)` row
    // merged in and level numbers replaced by their mnemonics) so the
    // shared label-width and count-width calculations cover the rows
    // we'll actually render.  Otherwise the column widths would lag
    // the labels and the bars would shift between fields.
    let field_rows: Vec<Vec<(String, u64)>> =
        summary.fields.iter().map(field_display_rows).collect();

    let label_width = compute_label_width(summary, &field_rows);
    let count_width = compute_count_width(summary, &field_rows);

    for (field, rows) in summary.fields.iter().zip(&field_rows) {
        out.push(String::new());
        out.push(format!(
            "== {} ({} event{}) ==",
            field.name,
            field.event_count,
            if field.event_count == 1 { "" } else { "s" },
        ));
        let max = rows.iter().map(|(_, c)| *c).max().unwrap_or(0);
        for (value, count) in rows {
            out.push(format_histogram_row(
                value,
                *count,
                max,
                label_width,
                count_width,
            ));
        }
        if field.other_distinct > 0 {
            out.push(format!(
                "    (plus {} less common value{} totalling {} event{})",
                field.other_distinct,
                if field.other_distinct == 1 { "" } else { "s" },
                field.other_count,
                if field.other_count == 1 { "" } else { "s" },
            ));
        }
    }

    if let Some(time) = &summary.time {
        out.push(String::new());
        out.push(format!(
            "== time ({} bucket{}, {} per bucket) ==",
            time.buckets.len(),
            if time.buckets.len() == 1 { "" } else { "s" },
            time.bucket_label,
        ));
        let max = time.buckets.iter().map(|(_, c)| *c).max().unwrap_or(0);
        for (start, count) in &time.buckets {
            let label = format_time_label(*start, time.bucket_label);
            out.push(format_histogram_row(
                &label,
                *count,
                max,
                label_width,
                count_width,
            ));
        }
    }

    out
}

/// Builds the (label, count) rows that get rendered for one field's
/// histogram.  Three transforms over the raw [`FieldSummary::values`]:
///
/// 1. Per-field display tweaks — today, only `level` numbers turn into
///    their mnemonic (`30` → `INFO`).
/// 2. The `(no value)` row is merged in when `no_value_count > 0`.
/// 3. Everything is re-sorted by count (desc), label (asc) so a high
///    `(no value)` count appears at the top of a sparsely-populated
///    field.
fn field_display_rows(field: &FieldSummary) -> Vec<(String, u64)> {
    let mut rows: Vec<(String, u64)> = field
        .values
        .iter()
        .map(|(v, c)| (display_value(&field.name, v), *c))
        .collect();
    if field.no_value_count > 0 {
        rows.push((NO_VALUE_LABEL.to_string(), field.no_value_count));
    }
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    rows
}

/// Returns the display label for a (field, canonical-JSON-value) pair.
/// The default is the canonical JSON form (matching what
/// [`crate::render::format_event`] shows for extras).  Special cases:
///
/// - `level`: replace bunyan numbers with their mnemonic (`30` →
///   `INFO`, etc.) so the histogram reads the same way the per-record
///   view does.  Falls back to the raw form if the value isn't a known
///   number.
fn display_value(field: &str, canonical: &str) -> String {
    if field == "level"
        && let Ok(n) = canonical.parse::<u8>()
        && let Ok(level) = crate::event::Level::try_from(n)
    {
        return level.as_str().to_string();
    }
    canonical.to_string()
}

fn compute_label_width(
    summary: &Summary,
    field_rows: &[Vec<(String, u64)>],
) -> usize {
    let mut w = 0;
    for rows in field_rows {
        for (label, _) in rows {
            w = w.max(display_width(label));
        }
    }
    if let Some(time) = &summary.time {
        for (start, _) in &time.buckets {
            w = w.max(display_width(&format_time_label(*start, time.bucket_label)));
        }
    }
    // Cap so a single very long label doesn't squeeze the bar: long
    // labels overflow their column rather than steal space from the bar.
    w.min(40)
}

fn compute_count_width(
    summary: &Summary,
    field_rows: &[Vec<(String, u64)>],
) -> usize {
    let mut max_count: u64 = 0;
    for rows in field_rows {
        for (_, c) in rows {
            max_count = max_count.max(*c);
        }
    }
    if let Some(time) = &summary.time {
        for (_, c) in &time.buckets {
            max_count = max_count.max(*c);
        }
    }
    max_count.to_string().len()
}

fn format_histogram_row(
    label: &str,
    count: u64,
    max: u64,
    label_width: usize,
    count_width: usize,
) -> String {
    let bar = render_bar(count, max, HISTOGRAM_BAR_WIDTH);
    // Label is right-aligned so values of varying length still align
    // their `|` separators.  When a label exceeds the cap, let it run
    // long rather than truncating — preserving accuracy is more
    // important than perfect column alignment.
    let pad = label_width.saturating_sub(display_width(label));
    let padding = " ".repeat(pad);
    let bar_pad = HISTOGRAM_BAR_WIDTH.saturating_sub(display_width(&bar));
    format!(
        "    {padding}{label} |{bar}{} {count:>count_width$}",
        " ".repeat(bar_pad),
    )
}

fn format_time_label(start: DateTime<Utc>, bucket_label: &str) -> String {
    match bucket_label {
        "1d" => start.format("%Y-%m-%d").to_string(),
        "1h" => start.format("%Y-%m-%dT%H").to_string(),
        // 1m and any future smaller buckets show minute granularity.
        _ => start.format("%Y-%m-%dT%H:%M").to_string(),
    }
}

/// Width in display cells of `s`.  Treats every char as one cell, which
/// is correct for the ASCII labels we emit (field names, JSON values
/// for the typical case, RFC3339 fragments) and conservative for the
/// histogram block characters (each is one cell).  A future
/// double-width-aware version would pull in unicode-width.
fn display_width(s: &str) -> usize {
    s.chars().count()
}

/// Builds a Unicode-block histogram bar sized to `width` cells.  `max`
/// is the largest count in the histogram (so the bar is normalized
/// across rows); a row with `count == max` uses the full width.  Sub-
/// cell precision uses the LEFT-N-EIGHTHS family (`▏ ▎ ▍ ▌ ▋ ▊ ▉`).
fn render_bar(count: u64, max: u64, width: usize) -> String {
    if max == 0 || width == 0 {
        return String::new();
    }
    let total_eighths = (width as u64) * 8;
    // Use u128 to avoid overflow on multi-billion-event histograms.
    let scaled =
        ((u128::from(count) * u128::from(total_eighths)) / u128::from(max)) as u64;
    let full = (scaled / 8) as usize;
    let rem = (scaled % 8) as usize;
    let mut out = String::with_capacity(full + 1);
    for _ in 0..full {
        out.push('█');
    }
    if rem > 0 {
        let partial = match rem {
            1 => '▏',
            2 => '▎',
            3 => '▍',
            4 => '▌',
            5 => '▋',
            6 => '▊',
            7 => '▉',
            _ => unreachable!(),
        };
        out.push(partial);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{TestDir, append_bunyan, append_bunyan_at};
    use chrono::TimeZone;
    use slog::info;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid timestamp")
    }

    #[test]
    fn empty_engine_summary_has_no_fields_or_time() {
        let engine = Engine::new();
        let s = summarize(&engine, &Filter::default());
        assert_eq!(s.total_events, 0);
        assert!(s.fields.is_empty());
        assert!(s.time.is_none());
    }

    #[test]
    fn summarize_counts_fields_and_values() {
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        // Three Nexus, one sled-agent.  Two distinct messages on Nexus.
        append_bunyan_at(&p, "Nexus", t(10), "starting");
        append_bunyan_at(&p, "Nexus", t(20), "starting");
        append_bunyan_at(&p, "Nexus", t(30), "running");
        append_bunyan_at(&p, "sled-agent", t(40), "running");

        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let s = summarize(&engine, &Filter::default());

        assert_eq!(s.total_events, 4);
        // `name` should appear with two values: Nexus(3) > sled-agent(1).
        let name = s.fields.iter().find(|f| f.name == "name").unwrap();
        assert_eq!(name.event_count, 4);
        assert_eq!(name.values[0], (r#""Nexus""#.to_string(), 3));
        assert_eq!(name.values[1], (r#""sled-agent""#.to_string(), 1));

        // `msg` should appear with starting(2) > running(2): tie broken
        // by alphabetical value.  "running" > "starting" because the
        // JSON form compares including the quotes — both lead with `"`,
        // so the rest sorts as plain strings: "running" < "starting".
        let msg = s.fields.iter().find(|f| f.name == "msg").unwrap();
        assert_eq!(msg.event_count, 4);
        assert_eq!(
            msg.values,
            vec![
                (r#""running""#.to_string(), 2),
                (r#""starting""#.to_string(), 2),
            ],
        );

        dir.cleanup();
    }

    #[test]
    fn time_field_excluded_from_field_list() {
        // Even though `time` is a top-level field on every record, the
        // builder strips it so the time-bucket histogram is the
        // canonical place it shows up.  Without this carve-out it
        // would dominate the per-source top-K with one bucket per
        // distinct timestamp.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan_at(&p, "x", t(10), "a");
        append_bunyan_at(&p, "x", t(20), "b");
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let s = summarize(&engine, &Filter::default());
        assert!(s.fields.iter().all(|f| f.name != "time"));
        assert!(s.time.is_some());
    }

    #[test]
    fn summarize_respects_event_filter() {
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan_at(&p, "Nexus", t(10), "starting");
        append_bunyan_at(&p, "sled-agent", t(20), "starting");
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let filter: Filter = "name=Nexus".parse().unwrap();
        let s = summarize(&engine, &filter);
        assert_eq!(s.total_events, 1);
        let name = s.fields.iter().find(|f| f.name == "name").unwrap();
        assert_eq!(name.values, vec![(r#""Nexus""#.to_string(), 1)]);
        dir.cleanup();
    }

    #[test]
    fn summarize_unions_top_fields_per_source() {
        // Source A has fields [name, msg, hostname, pid, level, v,
        // alpha,beta,gamma,delta,epsilon,zeta] — that's 11 non-time
        // fields, only 10 of which survive A's per-source top-K.
        // Source B contributes a different 10 that picks up `eta` (which
        // A's cut would drop).  The union should include `eta` even
        // though A's only event with `eta` falls below A's cutoff.
        //
        // We choose value distributions so the per-source counts force
        // a deterministic ordering: in source A, alpha..zeta each
        // appear twice; in source B alpha..zeta also each appear
        // twice but `eta` appears 5 times (so B keeps it).
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        // Build A with 12 events that each carry one extra so per-event
        // counts of name/msg/etc are inflated and the extras don't
        // dominate.
        append_bunyan(&a, "A", |log| {
            for _ in 0..2 {
                info!(log, "m"; "alpha" => 1, "beta" => 1, "gamma" => 1,
                                "delta" => 1, "epsilon" => 1, "zeta" => 1);
            }
            info!(log, "m"; "eta" => 1);
        });
        append_bunyan(&b, "B", |log| {
            for _ in 0..5 {
                info!(log, "m"; "eta" => 1);
            }
            info!(log, "m"; "alpha" => 1);
        });
        let mut engine = Engine::new();
        engine.add_file_source(&a).unwrap();
        engine.add_file_source(&b).unwrap();
        let s = summarize(&engine, &Filter::default());
        assert!(s.fields.iter().any(|f| f.name == "eta"));
        dir.cleanup();
    }

    #[test]
    fn summarize_caps_field_values_to_top_n() {
        // 22 distinct messages, each appearing once; only 20 should
        // survive in the values list and the rest should be summarized
        // in `other_count`/`other_distinct`.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        for i in 0..(TOP_VALUES_PER_FIELD as i64 + 2) {
            append_bunyan_at(&p, "x", t(i), &format!("msg-{i:02}"));
        }
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let s = summarize(&engine, &Filter::default());
        let msg = s.fields.iter().find(|f| f.name == "msg").unwrap();
        assert_eq!(msg.values.len(), TOP_VALUES_PER_FIELD);
        assert_eq!(msg.other_distinct, 2);
        assert_eq!(msg.other_count, 2);
        dir.cleanup();
    }

    #[test]
    fn time_bucket_size_picks_minutes_for_short_range() {
        let (d, label) = pick_bucket_size(Duration::minutes(20));
        assert_eq!(label, "1m");
        assert_eq!(d, Duration::minutes(1));
    }

    #[test]
    fn time_bucket_size_picks_hours_for_day_range() {
        let (d, label) = pick_bucket_size(Duration::hours(30));
        assert_eq!(label, "1h");
        assert_eq!(d, Duration::hours(1));
    }

    #[test]
    fn time_bucket_size_picks_days_for_month_range() {
        let (d, label) = pick_bucket_size(Duration::days(30));
        assert_eq!(label, "1d");
        assert_eq!(d, Duration::days(1));
    }

    #[test]
    fn time_bucket_size_zero_range_picks_minutes() {
        // A single observed event has zero range; we should still get
        // exactly one bucket of the smallest unit so the histogram has
        // one usable row.
        let (_d, label) = pick_bucket_size(Duration::seconds(0));
        assert_eq!(label, "1m");
    }

    #[test]
    fn time_buckets_align_to_unit_and_cover_range() {
        // Three events spread across two minute buckets.  Expect two
        // buckets, the first counting one event and the second counting
        // two, both starting on minute boundaries.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        // 12:34:10 and 12:35:05 / 12:35:55: bucket boundaries at 12:34
        // and 12:35.
        append_bunyan_at(&p, "x", t(12 * 3600 + 34 * 60 + 10), "a");
        append_bunyan_at(&p, "x", t(12 * 3600 + 35 * 60 + 5), "b");
        append_bunyan_at(&p, "x", t(12 * 3600 + 35 * 60 + 55), "c");
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let s = summarize(&engine, &Filter::default());
        let time = s.time.unwrap();
        assert_eq!(time.bucket_label, "1m");
        assert_eq!(time.buckets.len(), 2);
        assert_eq!(time.buckets[0].1, 1);
        assert_eq!(time.buckets[1].1, 2);
        // Bucket starts on minute boundaries: 12:34:00, 12:35:00.
        assert_eq!(time.buckets[0].0.timestamp() % 60, 0);
        assert_eq!(time.buckets[1].0.timestamp() % 60, 0);
        assert_eq!(
            time.buckets[1].0.timestamp() - time.buckets[0].0.timestamp(),
            60,
        );
        dir.cleanup();
    }

    #[test]
    fn time_buckets_fill_in_gaps() {
        // Events at minute 0 and minute 5 produce six buckets (0..=5),
        // four of which are empty.  Histogram readers see the lull
        // explicitly rather than the whole gap collapsing out.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan_at(&p, "x", t(0), "a");
        append_bunyan_at(&p, "x", t(5 * 60), "b");
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let s = summarize(&engine, &Filter::default());
        let time = s.time.unwrap();
        assert_eq!(time.buckets.len(), 6);
        let counts: Vec<_> = time.buckets.iter().map(|(_, c)| *c).collect();
        assert_eq!(counts, vec![1, 0, 0, 0, 0, 1]);
        dir.cleanup();
    }

    #[test]
    fn render_bar_zero_max_returns_empty() {
        assert_eq!(render_bar(0, 0, 10), "");
    }

    #[test]
    fn render_bar_full_at_max() {
        assert_eq!(render_bar(10, 10, 5), "█████");
    }

    #[test]
    fn render_bar_partial_eighths() {
        // count == max/2 → half the bar.  Width 8 → 4 full blocks.
        assert_eq!(render_bar(50, 100, 8), "████");
        // count tiny relative to max → at least one eighth visible.
        // 1/100 * 8 cells * 8 eighths = 0.64 → rounds to 0 (no bar).
        assert_eq!(render_bar(1, 100, 8), "");
        // 2/100 over 8 cells = 1.28 eighths → one ▏ block.
        assert_eq!(render_bar(2, 100, 8), "▏");
    }

    #[test]
    fn format_summary_includes_total_and_field_sections() {
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan_at(&p, "Nexus", t(10), "go");
        append_bunyan_at(&p, "Nexus", t(20), "go");
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let s = summarize(&engine, &Filter::default());
        let out = format_summary(&s);
        assert!(out.iter().any(|l| l.starts_with("Summary: 2 events")));
        assert!(out.iter().any(|l| l.starts_with("== name")));
        assert!(out.iter().any(|l| l.starts_with("== msg")));
        assert!(out.iter().any(|l| l.starts_with("== time")));
    }

    #[test]
    fn format_summary_empty_engine_says_no_events() {
        let s = summarize(&Engine::new(), &Filter::default());
        let out = format_summary(&s);
        assert!(out.iter().any(|l| l.contains("no events")));
    }

    #[test]
    fn no_value_count_tracks_missing_field() {
        // `build` is on the first event but not the second; the
        // `(no value)` count should be 1 there, while always-present
        // core fields like `name` get 0.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan(&p, "x", |log| {
            info!(log, "m"; "build" => "0.1.0");
            info!(log, "m");
        });
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let s = summarize(&engine, &Filter::default());
        let build = s.fields.iter().find(|f| f.name == "build").unwrap();
        assert_eq!(build.event_count, 1);
        assert_eq!(build.no_value_count, 1);
        let name = s.fields.iter().find(|f| f.name == "name").unwrap();
        assert_eq!(name.no_value_count, 0);
        dir.cleanup();
    }

    #[test]
    fn format_summary_renders_no_value_row_for_optional_fields() {
        // Two events; only one carries a `build` extra.  The rendered
        // histogram for `build` must include a `(no value)` row, and
        // the count must be 1.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan(&p, "x", |log| {
            info!(log, "m"; "build" => "0.1.0");
            info!(log, "m");
        });
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let s = summarize(&engine, &Filter::default());
        let out = format_summary(&s);
        // Find the `build` section and the next blank line; the
        // `(no value)` row sits between them.
        let build_header = out
            .iter()
            .position(|l| l.starts_with("== build"))
            .expect("build section present");
        let no_value_row = out[build_header..]
            .iter()
            .find(|l| l.contains("(no value)"))
            .expect("(no value) row present in build section");
        assert!(
            no_value_row.contains(" 1"),
            "expected count of 1 in `(no value)` row, got: {no_value_row}",
        );
    }

    #[test]
    fn format_summary_omits_no_value_row_for_universal_fields() {
        // `name` is on every event (it's a bunyan core field); the
        // histogram should NOT include a `(no value)` row, since
        // showing one with count 0 is just noise.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan_at(&p, "Nexus", t(10), "go");
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let s = summarize(&engine, &Filter::default());
        let out = format_summary(&s);
        let name_header = out
            .iter()
            .position(|l| l.starts_with("== name"))
            .expect("name section present");
        // Section ends at the next blank line (or end of output).
        let section_end = out[name_header + 1..]
            .iter()
            .position(|l| l.is_empty())
            .map(|p| name_header + 1 + p)
            .unwrap_or(out.len());
        let body = &out[name_header..section_end];
        assert!(
            !body.iter().any(|l| l.contains("(no value)")),
            "name section should not include `(no value)`; got:\n{}",
            body.join("\n"),
        );
        dir.cleanup();
    }

    #[test]
    fn no_value_row_sorts_with_values_by_count() {
        // Three events: only one has the optional field.  In the
        // displayed rows, `(no value)` (count 2) ranks above the
        // present value (count 1).
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan(&p, "x", |log| {
            info!(log, "m"; "tag" => "alpha");
            info!(log, "m");
            info!(log, "m");
        });
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let s = summarize(&engine, &Filter::default());
        let tag = s.fields.iter().find(|f| f.name == "tag").unwrap();
        let rows = field_display_rows(tag);
        assert_eq!(rows[0].0, "(no value)");
        assert_eq!(rows[0].1, 2);
        assert_eq!(rows[1].0, r#""alpha""#);
        assert_eq!(rows[1].1, 1);
        dir.cleanup();
    }

    #[test]
    fn level_values_render_as_mnemonic() {
        // Mixed-level events: histogram should label rows `INFO` /
        // `ERROR` rather than `30` / `50`, matching the per-record
        // view's level rendering.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan(&p, "x", |log| {
            info!(log, "i1");
            slog::error!(log, "e1");
            info!(log, "i2");
        });
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let s = summarize(&engine, &Filter::default());
        let level = s.fields.iter().find(|f| f.name == "level").unwrap();
        let rows = field_display_rows(level);
        // Two INFO, one ERROR; sorted desc by count.
        assert_eq!(rows[0].0, "INFO");
        assert_eq!(rows[0].1, 2);
        assert_eq!(rows[1].0, "ERROR");
        assert_eq!(rows[1].1, 1);
        // Underlying canonical form is still numeric so consumers that
        // want the bunyan number (e.g. for filter construction later)
        // can recover it.
        assert_eq!(level.values[0].0, "30");
        dir.cleanup();
    }

    #[test]
    fn format_summary_level_section_uses_mnemonics() {
        // End-to-end: the rendered output for the `level` field must
        // contain the mnemonic, not the numeric form.
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan(&p, "x", |log| {
            info!(log, "m");
            slog::warn!(log, "m");
        });
        let mut engine = Engine::new();
        engine.add_file_source(&p).unwrap();
        let s = summarize(&engine, &Filter::default());
        let out = format_summary(&s);
        let level_header = out
            .iter()
            .position(|l| l.starts_with("== level"))
            .expect("level section present");
        let section_end = out[level_header + 1..]
            .iter()
            .position(|l| l.is_empty())
            .map(|p| level_header + 1 + p)
            .unwrap_or(out.len());
        let body = out[level_header..section_end].join("\n");
        assert!(body.contains("INFO"), "expected INFO label, got:\n{body}");
        assert!(body.contains("WARN"), "expected WARN label, got:\n{body}");
        assert!(
            !body.contains(" 30 "),
            "level histogram should not include numeric levels in row labels:\n{body}",
        );
    }

    #[test]
    fn display_value_passes_through_non_level_fields() {
        // Sanity: non-level fields keep their canonical JSON form.
        assert_eq!(display_value("name", r#""Nexus""#), r#""Nexus""#);
        assert_eq!(display_value("v", "0"), "0");
        assert_eq!(display_value("level", "30"), "INFO");
        // An unrecognized "level" value falls back to the raw form
        // rather than panicking — protects against future bunyan
        // levels we haven't taught the table.
        assert_eq!(display_value("level", "99"), "99");
        assert_eq!(display_value("level", "not-a-number"), "not-a-number");
    }
}
