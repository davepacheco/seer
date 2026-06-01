// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Field and time histograms over an [`Engine`]'s events.
//!
//! A [`Summary`] is the data behind a Summary tab: for the events that
//! pass the active filter, it captures:
//!
//! - the most-frequent top-level JSON field names across all sources
//!   (capped at [`TOP_FIELDS`]) and, for each, the most common values
//!   that field takes;
//! - a histogram of event counts in time buckets sized so that the full
//!   range is divided into roughly 30 buckets (1m / 1h / 1d).
//!
//! The Summary is computed in a single pass: every event contributes to
//! every field's value count and to one time bucket; afterwards the
//! global top-K by total count picks which fields survive and in what
//! order they're displayed.  Keeping all per-field value counts during
//! the pass costs more memory than a two-pass design that learns the
//! top-K first and only keeps values for those fields, but we already
//! pay the cost of walking the events once on the way to the histogram
//! and a single hash map per field is small in practice.
//!
//! The `time` field is handled specially: it never appears in the field
//! list (it would dominate it with one bucket per RFC3339 timestamp).
//! Its place is taken by the time-bucket histogram.
//!
//! The summary is purely a data type — formatting it into display lines
//! lives in [`format_summary`], which turns it into the same
//! `Vec<String>` shape that the regular log view uses so the TUI's
//! viewport, search, and rendering paths can be reused unchanged.

use crate::event::Event;
use crate::filter::Filter;
use crate::position::Cursor;
use crate::{Direction, engine::Engine};
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

/// How many fields we keep in the summary, globally across all sources.
const TOP_FIELDS: usize = 25;

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
    /// Length is at most [`TOP_FIELDS`].
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
    /// Bucket granularity chosen for this summary.  Carries both the
    /// width (via [`TimeBucket::duration`]) and the short label
    /// rendered in the histogram header (via [`TimeBucket::label`]).
    pub bucket: TimeBucket,
    /// Buckets ordered by start time, ascending.  Empty buckets in the
    /// middle of the range are present with `count = 0` so the
    /// histogram shows quiet periods rather than silently compressing
    /// them out.
    pub buckets: Vec<(DateTime<Utc>, u64)>,
}

/// Granularity of one bar in the time histogram.
///
/// Variants are listed in ascending order of [`Self::duration`]; the
/// finest granularity is returned by [`Self::min_granularity`] and is
/// what [`SummaryBuilder`] buckets every event at during the pass.
/// Coarser granularities are derived from the fine-grained counts at
/// finish time.
///
/// When adding a new variant, keep the variant list in ascending order
/// of duration and update [`Self::all`].  The `time_bucket_*` unit
/// tests verify both invariants against `strum::IntoEnumIterator`, so
/// drift fails the test suite rather than the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(strum::EnumIter))]
pub enum TimeBucket {
    Minute,
    Hour,
    Day,
}

impl TimeBucket {
    /// All variants, in ascending order of duration.
    ///
    /// Used by the runtime paths that need to enumerate buckets
    /// without pulling `strum` into the non-test build.  The
    /// `time_bucket_all_matches_iteration` unit test keeps this in
    /// sync with `strum::IntoEnumIterator`.
    const fn all() -> &'static [Self] {
        &[Self::Minute, Self::Hour, Self::Day]
    }

    /// Returns the width of one bucket at this granularity.
    pub fn duration(self) -> Duration {
        match self {
            Self::Minute => Duration::minutes(1),
            Self::Hour => Duration::hours(1),
            Self::Day => Duration::days(1),
        }
    }

    /// Returns the short label shown in the histogram header
    /// (`"1m"`, `"1h"`, `"1d"`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Minute => "1m",
            Self::Hour => "1h",
            Self::Day => "1d",
        }
    }

    /// Rounds `t` down to the nearest bucket boundary at this
    /// granularity.  Handles negative timestamps correctly (floor,
    /// not truncation toward zero) so a UTC timestamp before 1970 —
    /// vanishingly unlikely in our domain, but cheap to get right —
    /// still falls into a stable bucket.
    pub fn floor(self, t: DateTime<Utc>) -> DateTime<Utc> {
        let secs = self.duration().num_seconds().max(1);
        let aligned = t.timestamp() - t.timestamp().rem_euclid(secs);
        DateTime::from_timestamp(aligned, 0).expect("aligned timestamp")
    }

    /// Formats `t` as the row label appropriate for this granularity.
    pub fn format_label(self, t: DateTime<Utc>) -> String {
        match self {
            Self::Minute => t.format("%Y-%m-%dT%H:%M").to_string(),
            Self::Hour => t.format("%Y-%m-%dT%H").to_string(),
            Self::Day => t.format("%Y-%m-%d").to_string(),
        }
    }

    /// Returns the finest supported granularity.
    ///
    /// Observers bucket every event at this granularity during the
    /// pass; coarser granularities are derived from the fine-grained
    /// counts at finish time.  This is what bounds the builder's
    /// memory use: instead of holding one timestamp per event, the
    /// builder holds at most one count per distinct fine-grained
    /// bucket the events fall into.
    pub fn min_granularity() -> Self {
        Self::all()[0]
    }

    /// Picks the bucket size whose count for `range` is closest to
    /// about 30 buckets.  Ties prefer the smaller granularity so a
    /// flat range never collapses into one huge bucket.
    pub fn pick_for_range(range: Duration) -> Self {
        let target = 30i64;
        let range_secs = range.num_seconds().max(0);
        let mut best = Self::min_granularity();
        let mut best_dist = i64::MAX;
        for &cand in Self::all() {
            let unit_secs = cand.duration().num_seconds().max(1);
            // At least one bucket: a zero-or-negative range still
            // produces a single bucket containing the lone observed
            // timestamp.
            let n = (range_secs / unit_secs).max(1);
            let dist = (n - target).abs();
            if dist < best_dist {
                best = cand;
                best_dist = dist;
            }
        }
        best
    }
}

/// Returns a [`Summary`] of every event in `engine` that passes
/// `filter`.
///
/// Traverses the merged event stream once via a [`Stepper`].  Source-id
/// filtering is applied at the engine level (so excluded sources are
/// never opened); event-level predicates are applied per record by the
/// stepper.  Parse errors are skipped — the summary describes what was
/// successfully parsed and accepted by the filter.
pub fn summarize(engine: &Engine, filter: &Filter) -> Summary {
    let mut stepper = engine.stepper(filter.clone(), &Cursor::new());
    let mut builder = SummaryBuilder::default();
    while let Some(record) = stepper.step_forward() {
        if let Ok(event) = record.event() {
            builder.observe(event);
        }
    }

    // The stepper here has no per-fill records-to-scan budget, so
    // `step_forward` returns `None` only at true EOF.
    assert!(stepper.is_exhausted(Direction::Forward));

    builder.finish()
}

/// Streaming accumulator for a [`Summary`].
///
/// Public so callers that want incremental control over the pass
/// (e.g. the TUI, which drives a stepper one record at a time so the
/// outer wall-clock budget controls how long each tick spends folding)
/// can feed matching events into the same accumulator the convenience
/// [`summarize`] function uses.  Keeps everything in memory until
/// [`Self::finish`] trims to the top-K shapes.
#[derive(Default)]
pub struct SummaryBuilder {
    total_events: u64,
    /// Per-field, per-value count.  Populated for every field
    /// encountered, regardless of whether the field will eventually
    /// make the top-K — pruning happens at finish time.
    field_value_counts: HashMap<String, HashMap<String, u64>>,
    /// Per-field total occurrence count across all events.  Used both
    /// to pick the surviving top-K and to order them in the final
    /// summary, so the displayed membership and ordering criteria
    /// agree.
    field_total_counts: HashMap<String, u64>,
    /// Earliest and latest event timestamp observed.  Used to size the
    /// time-bucket histogram.
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// Event counts pre-bucketed at [`TimeBucket::min_granularity`].
    /// Bounds memory at O(distinct fine-grained buckets) rather than
    /// O(total events): real Oxide log spans cover hours to a few
    /// days, so the map size is in the thousands even when the event
    /// count is in the millions.  The display bucket size isn't known
    /// until [`Self::finish`] sees the full range, so coarser
    /// granularities are aggregated up from these counts at that
    /// point.
    fine_buckets: HashMap<DateTime<Utc>, u64>,
}

impl SummaryBuilder {
    /// Folds one event into the accumulator.  Caller is responsible for
    /// deciding whether the event passed the filter.
    pub fn observe(&mut self, event: &Event) {
        self.total_events += 1;
        // Update time bookkeeping first so we can drop the timestamp
        // before iterating fields (since `time` is excluded from the
        // field list).
        let fine = TimeBucket::min_granularity().floor(event.time);
        *self.fine_buckets.entry(fine).or_default() += 1;
        match self.time_range {
            None => self.time_range = Some((event.time, event.time)),
            Some((min, max)) => {
                self.time_range =
                    Some((min.min(event.time), max.max(event.time)));
            }
        }
        for (name, value) in iter_fields(event) {
            // Time is recorded separately above; skip it here so it
            // doesn't crowd out other fields in the global top-K.
            if name == "time" {
                continue;
            }
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
        // Pick the top-K fields by total count, descending; break ties
        // by name ascending for determinism.  The same ordering is used
        // for display, so the rendered membership and ordering criteria
        // agree.
        let mut by_count: Vec<(String, u64)> =
            self.field_total_counts.into_iter().collect();
        by_count.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        by_count.truncate(TOP_FIELDS);
        let total_events = self.total_events;
        let fields = by_count
            .into_iter()
            .map(|(name, _)| {
                let value_counts = self
                    .field_value_counts
                    .get(&name)
                    .cloned()
                    .unwrap_or_default();
                build_field_summary(name, value_counts, total_events)
            })
            .collect();

        let time = self.time_range.map(|(min, max)| {
            let bucket = TimeBucket::pick_for_range(max - min);
            build_time_summary(bucket, min, max, &self.fine_buckets)
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
    bucket: TimeBucket,
    min: DateTime<Utc>,
    max: DateTime<Utc>,
    fine_buckets: &HashMap<DateTime<Utc>, u64>,
) -> TimeSummary {
    let bucket_secs = bucket.duration().num_seconds().max(1);
    // Align min and max to display-bucket boundaries.  The number of
    // buckets is (end-start)/unit + 1 so the bucket containing `max`
    // itself is included; this is what makes "events at minute 0 and
    // minute 5" produce six buckets, not five.
    let start = bucket.floor(min);
    let end = bucket.floor(max);
    let n = ((end.timestamp() - start.timestamp()) / bucket_secs) as usize + 1;
    let mut counts = vec![0u64; n];
    // Aggregate fine-grained counts up to the display granularity.
    // `fine` is already floored to the minimum granularity, so we only
    // need to re-floor when the display bucket is coarser.
    for (&fine, &count) in fine_buckets {
        let display = bucket.floor(fine);
        let idx =
            ((display.timestamp() - start.timestamp()) / bucket_secs) as usize;
        // Guard against an out-of-range index from a future caller
        // that passes a map not bounded by `min`/`max`.  In the
        // current code path the arithmetic above is always in range.
        if idx < counts.len() {
            counts[idx] += count;
        }
    }
    let buckets = (0..n)
        .map(|i| {
            let bucket_start_secs =
                start.timestamp() + (i as i64) * bucket_secs;
            let bucket_start = DateTime::from_timestamp(bucket_start_secs, 0)
                .expect("aligned timestamp");
            (bucket_start, counts[i])
        })
        .collect();
    TimeSummary { bucket, buckets }
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
    let mut out: Vec<(String, serde_json::Value)> =
        Vec::with_capacity(7 + event.extra.len());
    // unwrap(): all of these have serialize impls that should never fail
    out.push(("time".to_string(), serde_json::to_value(&event.time).unwrap()));
    out.push((
        "level".to_string(),
        serde_json::to_value(&event.level).unwrap(),
    ));
    out.push(("name".to_string(), serde_json::to_value(&event.name).unwrap()));
    out.push((
        "hostname".to_string(),
        serde_json::to_value(&event.hostname).unwrap(),
    ));
    out.push(("pid".to_string(), serde_json::to_value(&event.pid).unwrap()));
    out.push(("msg".to_string(), serde_json::to_value(&event.msg).unwrap()));
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
            time.bucket.label(),
        ));
        let max = time.buckets.iter().map(|(_, c)| *c).max().unwrap_or(0);
        for (start, count) in &time.buckets {
            let label = time.bucket.format_label(*start);
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
            w = w.max(display_width(&time.bucket.format_label(*start)));
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
    let scaled = ((u128::from(count) * u128::from(total_eighths))
        / u128::from(max)) as u64;
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
    use crate::test_fixtures::{TestDir, append_bunyan, append_bunyan_at, t};
    use slog::info;

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
    fn summarize_top_fields_are_global() {
        // The surviving field set is the global top-K by total count,
        // bounded by `TOP_FIELDS`.  Membership and ordering use the same
        // criterion (global count desc), so a field whose count places
        // it inside the top-K is included regardless of which source it
        // came from, and a field below the cutoff is excluded even if
        // it would be popular within some small source.
        //
        // Setup: source A carries `TOP_FIELDS + 4` extras with strictly
        // decreasing frequencies so the cutoff bites cleanly and there
        // are no ties at the boundary.  Source B is small and
        // contributes a unique field `b_only` with a single occurrence,
        // well below the cutoff, to confirm that a small source no
        // longer rescues its locally-popular fields.
        //
        // We write the JSON lines directly via `append_raw` because
        // `slog::info!`'s key positions need `&'static str`, which
        // prevents driving the field names from a loop.
        use crate::test_fixtures::append_raw;
        let dir = TestDir::new();
        let a = dir.path().join("a.log");
        let b = dir.path().join("b.log");
        let n_extras = TOP_FIELDS + 4;
        let mut time = 0i64;
        for i in 0..n_extras {
            let name = format!("f_{i:02}");
            // `f_i` appears `n_extras - i` times so frequencies are
            // strictly decreasing and there are no ties.
            for _ in 0..(n_extras - i) {
                let line = serde_json::json!({
                    "v": 0,
                    "level": 30,
                    "name": "A",
                    "hostname": "test-host",
                    "pid": 42,
                    "time": t(time).to_rfc3339(),
                    "msg": "m",
                    &name: 1,
                });
                append_raw(&a, &line.to_string());
                time += 1;
            }
        }
        append_bunyan(&b, "B", |log| {
            info!(log, "m"; "b_only" => 1);
        });
        let mut engine = Engine::new();
        engine.add_file_source(&a).unwrap();
        engine.add_file_source(&b).unwrap();
        let s = summarize(&engine, &Filter::default());

        // Result is exactly `TOP_FIELDS`: more candidates exist than
        // the cap allows, and the strictly-decreasing frequencies mean
        // there's no ambiguity at the boundary.
        assert_eq!(s.fields.len(), TOP_FIELDS);

        // The 5 bunyan core fields (name, msg, hostname, pid, level)
        // appear on every event, so their global counts are the
        // highest and they always survive.
        for name in ["name", "msg", "hostname", "pid", "level"] {
            assert!(
                s.fields.iter().any(|f| f.name == name),
                "expected `{name}` in top-K, got: {:?}",
                s.fields.iter().map(|f| &f.name).collect::<Vec<_>>(),
            );
        }

        // Among the `f_i` extras, the highest-frequency ones survive
        // (low index = high count) and the lowest-frequency ones are
        // dropped.
        assert!(s.fields.iter().any(|f| f.name == "f_00"));
        let last_name = format!("f_{:02}", n_extras - 1);
        assert!(
            !s.fields.iter().any(|f| f.name == last_name),
            "expected `{last_name}` to fall outside top-K, got: {:?}",
            s.fields.iter().map(|f| &f.name).collect::<Vec<_>>(),
        );

        // `b_only` (1 occurrence in source B) is not surfaced — the
        // small source it came from no longer rescues its unique
        // fields.
        assert!(!s.fields.iter().any(|f| f.name == "b_only"));

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
        assert_eq!(
            TimeBucket::pick_for_range(Duration::minutes(20)),
            TimeBucket::Minute,
        );
    }

    #[test]
    fn time_bucket_size_picks_hours_for_day_range() {
        assert_eq!(
            TimeBucket::pick_for_range(Duration::hours(30)),
            TimeBucket::Hour,
        );
    }

    #[test]
    fn time_bucket_size_picks_days_for_month_range() {
        assert_eq!(
            TimeBucket::pick_for_range(Duration::days(30)),
            TimeBucket::Day,
        );
    }

    #[test]
    fn time_bucket_size_zero_range_picks_minutes() {
        // A single observed event has zero range; we should still get
        // exactly one bucket of the smallest unit so the histogram has
        // one usable row.
        assert_eq!(
            TimeBucket::pick_for_range(Duration::seconds(0)),
            TimeBucket::Minute,
        );
    }

    #[test]
    fn time_bucket_all_matches_iteration() {
        // Hand-rolled list in `TimeBucket::all` must match the order
        // and contents of `strum::EnumIter`.  This is what keeps
        // `strum` a dev-only dependency: the runtime code iterates
        // the hand-rolled list, but drift fails the test suite.
        use strum::IntoEnumIterator;
        let by_iter: Vec<TimeBucket> = TimeBucket::iter().collect();
        let by_hand: Vec<TimeBucket> = TimeBucket::all().to_vec();
        assert_eq!(by_iter, by_hand);
    }

    #[test]
    fn time_bucket_all_ordered_ascending_by_duration() {
        // `min_granularity` assumes the first element of `all` is the
        // shortest-duration variant.  Check that against the actual
        // durations rather than the source order.
        let durations: Vec<_> =
            TimeBucket::all().iter().map(|b| b.duration()).collect();
        let mut sorted = durations.clone();
        sorted.sort();
        assert_eq!(durations, sorted);
    }

    #[test]
    fn time_bucket_min_granularity_is_smallest() {
        use strum::IntoEnumIterator;
        let by_iter = TimeBucket::iter().min_by_key(|b| b.duration()).unwrap();
        assert_eq!(TimeBucket::min_granularity(), by_iter);
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
        assert_eq!(time.bucket, TimeBucket::Minute);
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
