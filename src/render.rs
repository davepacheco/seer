// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Formatting an [`Event`] into the lines that the CLI prints and the TUI
//! draws.
//!
//! The shape mirrors `looker --output long`: a header line built from the
//! bunyan core fields, followed by one indented `key = value` line per
//! additional structured field.  Timestamps render with millisecond
//! precision and a `Z` (UTC) suffix — bunyan's source files often carry
//! nanosecond precision, but for human triage the milliseconds are more
//! than enough and the trailing digits crowd the line.  No color: both
//! binaries emit either to a TTY-agnostic file or a ratatui buffer that
//! owns its own styling.
//!
//! Values are rendered as JSON via [`serde_json::Value`]'s `Display`
//! impl.  That preserves the type distinction between `"42"` and `42`,
//! between `true` and `"true"`, etc., which matters when triaging logs
//! whose schema isn't known in advance — looker drops the quotes for
//! readability, but for an explorer that lets you build filters from
//! what you see on screen, the unambiguous form is more useful.
//!
//! Every returned `String` is exactly one display row (no embedded
//! newlines).  An event with `n` extra fields produces `1 + n` lines.

use crate::event::Event;
use chrono::{DateTime, Utc};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// How [`format_event`] should render the bunyan `hostname` field.
///
/// Selected via the `h` field-display dialog in the TUI and persisted
/// on the host [`crate::stream::LogStream`] so the choice outlives a
/// session.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum HostnameDisplay {
    /// Trim to the first dot-component, then collapse a trailing UUID
    /// to its first 8-character group — see [`short_hostname`].  This
    /// is the default because real Oxide hostnames carry a domain
    /// suffix and a service-instance UUID that crowd the header
    /// without informing the user.
    #[default]
    Short,
    /// Render the hostname exactly as it appears in the event.
    Full,
    /// Don't render the hostname column at all (no field, no
    /// surrounding spaces).
    None,
}

/// Returns the "short" form of `hostname`:
///
/// 1. Trim everything after the first `.` (so `gimlet-01.oxide.test`
///    becomes `gimlet-01`).
/// 2. If what remains ends with a UUID (`8-4-4-4-12` lowercase or
///    uppercase hex), strip all but the first 8-hex group of the UUID
///    and the dash before it.  So
///    `oxz_nexus_c53300fc-84eb-490a-9e1e-9e18d372856d` becomes
///    `oxz_nexus_c53300fc`.
///
/// Inputs that don't match either rule are returned as-is.  The 8-hex
/// prefix of the UUID has to sit at a boundary (preceded by a non-hex
/// character or the start of the string) so an arbitrary string of hex
/// digits doesn't get mistaken for a UUID's first group.
pub fn short_hostname(hostname: &str) -> String {
    // (?x) puts the regex in extended mode, where unescaped whitespace
    // and `#` comments are ignored — keeps the multi-line form readable
    // without smuggling literal whitespace into the pattern.
    static UUID_TAIL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?x)
              ^
              ((?:.*[^0-9a-fA-F])?[0-9a-fA-F]{8})
              -[0-9a-fA-F]{4}
              -[0-9a-fA-F]{4}
              -[0-9a-fA-F]{4}
              -[0-9a-fA-F]{12}
              $",
        )
        .expect("static regex compiles")
    });
    let head = hostname.split_once('.').map(|(h, _)| h).unwrap_or(hostname);
    if let Some(caps) = UUID_TAIL.captures(head) {
        return caps[1].to_string();
    }
    head.to_string()
}

/// Formats a UTC timestamp the way both binaries display it.
///
/// With `show_date` true the result is a full ISO-8601 date-time at
/// millisecond precision (`2026-04-30T15:30:00.743Z`); with it false the
/// date prefix is stripped, leaving only `15:30:00.743Z`.  The `Z`
/// suffix is preserved either way so the value is still unambiguously
/// UTC when the user copies it out.
pub fn format_time(time: &DateTime<Utc>, show_date: bool) -> String {
    let pattern =
        if show_date { "%Y-%m-%dT%H:%M:%S%.3fZ" } else { "%H:%M:%S%.3fZ" };
    time.format(pattern).to_string()
}

/// Bundle of per-stream display knobs threaded through [`format_event`].
///
/// Lives in `render` rather than `stream` because it's the function
/// signature that needs the shape; [`crate::stream::LogStream`] holds
/// the same fields as its persisted state and hands a [`RenderOpts`]
/// over via [`crate::stream::LogStream::render_opts`].
///
/// Defaults match what a fresh stream renders: extras hidden, date
/// prefix shown, short hostname, name shown, pid hidden.  Pid is off by
/// default because Oxide processes typically restart often enough that
/// the number is noise; users opt in via the field-display dialog when
/// they need it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderOpts {
    pub show_extras: bool,
    pub show_date: bool,
    pub hostname: HostnameDisplay,
    pub show_pid: bool,
    pub show_name: bool,
    /// When true, the stream renders each record as its raw bytes from
    /// the source instead of the formatted header/extras layout.
    /// Bypasses every other field in this struct: raw mode shows the
    /// line as it appears on disk and ignores the column toggles.
    /// Toggled with `R` in the TUI; persisted on the host log stream.
    pub show_raw: bool,
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            show_extras: false,
            show_date: true,
            hostname: HostnameDisplay::Short,
            show_pid: false,
            show_name: true,
            show_raw: false,
        }
    }
}

/// Formats `event` into one or more display lines.
///
/// The first line is the bunyan header
/// (`<time> [<hostname>] [<name>[/<pid>]] <LEVEL> <msg>`); when
/// `opts.show_extras` is true, subsequent lines are
/// `    <key> = <json-value>`, one per entry in `event.extra`, ordered
/// by key.  Each of `show_date`, `hostname`, `show_pid`, and `show_name`
/// controls whether its column is rendered; the column (and any
/// adjacent separator) is omitted entirely when the field is hidden, so
/// no stray double-space artifacts appear.  The returned vec is
/// non-empty.
pub fn format_event(event: &Event, opts: &RenderOpts) -> Vec<String> {
    let cap = if opts.show_extras { 1 + event.extra.len() } else { 1 };
    let mut lines = Vec::with_capacity(cap);
    // Header column order: time, [hostname], [name[/pid]], level, msg.
    // Hostname moves ahead of name/pid so the eye lands on the machine
    // before the process; level sits adjacent to msg so the severity
    // reads next to its text.  Level is padded to 5 columns (the width
    // of `TRACE`/`DEBUG`/`ERROR`/`FATAL`) so the msg column lines up
    // across rows of mixed severity.  Each optional column is built
    // with a trailing space when it's present; absent columns
    // contribute the empty string so nothing collapses to a stray
    // double-space.
    let host_field = match opts.hostname {
        HostnameDisplay::Short => {
            // `as_ref::<str>` to pin which AsRef impl resolves —
            // Hostname forwards AsRef from its inner String, but a
            // blanket `AsRef<Self>` impl shadows it without an
            // annotation.
            let raw: &str = event.hostname.as_ref();
            format!("{} ", short_hostname(raw))
        }
        HostnameDisplay::Full => format!("{} ", event.hostname),
        HostnameDisplay::None => String::new(),
    };
    let proc_field = match (opts.show_name, opts.show_pid) {
        (true, true) => format!("{}/{} ", event.name, event.pid),
        (true, false) => format!("{} ", event.name),
        (false, true) => format!("{} ", event.pid),
        (false, false) => String::new(),
    };
    lines.push(format!(
        "{} {}{}{:<5} {}",
        format_time(&event.time, opts.show_date),
        host_field,
        proc_field,
        event.level,
        event.msg,
    ));
    if opts.show_extras {
        for (k, v) in &event.extra {
            lines.push(format!("    {k} = {v}"));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Event {
        serde_json::from_str(json).expect("test fixture parses as Event")
    }

    /// Test-only helper: pins pid and name *on* so the existing
    /// rendering tests keep exercising the fully-populated header
    /// they predate the pid/name toggles.  Dedicated tests below
    /// cover the toggle behavior itself.
    fn opts(
        show_extras: bool,
        show_date: bool,
        hostname: HostnameDisplay,
    ) -> RenderOpts {
        RenderOpts {
            show_extras,
            show_date,
            hostname,
            show_pid: true,
            show_name: true,
            show_raw: false,
        }
    }

    #[test]
    fn header_only_when_no_extras() {
        let e = parse(
            r#"{
                "v": 0,
                "level": 30,
                "name": "Nexus",
                "hostname": "ivanova",
                "pid": 15797,
                "time": "2026-05-07T04:48:12.142223551Z",
                "msg": "Nexus starting up"
            }"#,
        );
        let lines = format_event(&e, &opts(true, true, HostnameDisplay::Short));
        assert_eq!(lines.len(), 1);
        // Timestamp is truncated to milliseconds and printed with a `Z`
        // suffix, regardless of the source's nanosecond precision.
        // Hostname `ivanova` is single-component and not UUID-suffixed,
        // so `Short` leaves it unchanged.
        assert_eq!(
            lines[0],
            "2026-05-07T04:48:12.142Z ivanova \
             Nexus/15797 INFO  Nexus starting up",
        );
    }

    #[test]
    fn extras_render_indented_in_key_order() {
        // BTreeMap orders keys alphabetically, so the output is stable
        // regardless of the order the JSON parser visits them.
        let e = parse(
            r#"{
                "v": 0,
                "level": 30,
                "name": "Nexus",
                "hostname": "ivanova",
                "pid": 15797,
                "time": "2026-05-07T04:48:12.142Z",
                "msg": "Nexus starting up",
                "version": "0.1.0",
                "build": "0.1.0"
            }"#,
        );
        let lines = format_event(&e, &opts(true, true, HostnameDisplay::Short));
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with(" Nexus starting up"));
        assert_eq!(lines[1], r#"    build = "0.1.0""#);
        assert_eq!(lines[2], r#"    version = "0.1.0""#);
    }

    #[test]
    fn extras_hidden_when_show_extras_false() {
        // When `show_extras` is false the function returns only the
        // header, regardless of how many extras the event carries.
        let e = parse(
            r#"{
                "v": 0,
                "level": 30,
                "name": "Nexus",
                "hostname": "ivanova",
                "pid": 15797,
                "time": "2026-05-07T04:48:12.142Z",
                "msg": "Nexus starting up",
                "version": "0.1.0",
                "build": "0.1.0"
            }"#,
        );
        let lines =
            format_event(&e, &opts(false, true, HostnameDisplay::Short));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with(" Nexus starting up"));
    }

    #[test]
    fn header_format_pins_column_order() {
        // Pin the exact layout (time, hostname, name/pid, level, msg)
        // so accidental refactors don't drift the format.  The msg sits
        // last and runs to end-of-line, so any embedded spaces stay
        // intact.
        let e = parse(
            r#"{
                "v": 0,
                "level": 50,
                "name": "Nexus",
                "hostname": "host-a",
                "pid": 100,
                "time": "2026-05-07T00:00:00Z",
                "msg": "kaboom"
            }"#,
        );
        let header =
            &format_event(&e, &opts(true, true, HostnameDisplay::Short))[0];
        assert_eq!(
            header,
            "2026-05-07T00:00:00.000Z host-a Nexus/100 ERROR kaboom",
        );
    }

    #[test]
    fn show_date_false_drops_the_date_prefix() {
        // With `show_date = false` only the wall-clock part remains,
        // still at millisecond precision and still suffixed with `Z`.
        let e = parse(
            r#"{
                "v": 0,
                "level": 30,
                "name": "n",
                "hostname": "h",
                "pid": 1,
                "time": "2026-04-30T15:30:00.743Z",
                "msg": "m"
            }"#,
        );
        let header =
            &format_event(&e, &opts(false, false, HostnameDisplay::Short))[0];
        assert!(
            header.starts_with("15:30:00.743Z "),
            "expected leading time-only prefix, got {header:?}",
        );
        assert!(!header.contains("2026-04-30"));
    }

    #[test]
    fn hostname_full_keeps_dotted_and_uuid_suffixes() {
        // Full mode renders the hostname exactly as it sits in the
        // event — no dotted truncation, no UUID collapse.
        let e = parse(
            r#"{
                "v": 0,
                "level": 30,
                "name": "n",
                "hostname": "oxz_nexus_c53300fc-84eb-490a-9e1e-9e18d372856d.oxide.test",
                "pid": 1,
                "time": "2026-05-07T00:00:00Z",
                "msg": "m"
            }"#,
        );
        let header =
            &format_event(&e, &opts(false, true, HostnameDisplay::Full))[0];
        assert!(
            header.contains(
                " oxz_nexus_c53300fc-84eb-490a-9e1e-9e18d372856d.oxide.test \
                 n/1 ",
            ),
            "expected full hostname in header, got {header:?}",
        );
    }

    #[test]
    fn hostname_short_collapses_dotted_and_uuid_suffix() {
        let e = parse(
            r#"{
                "v": 0,
                "level": 30,
                "name": "n",
                "hostname": "oxz_nexus_c53300fc-84eb-490a-9e1e-9e18d372856d.oxide.test",
                "pid": 1,
                "time": "2026-05-07T00:00:00Z",
                "msg": "m"
            }"#,
        );
        let header =
            &format_event(&e, &opts(false, true, HostnameDisplay::Short))[0];
        assert!(
            header.contains(" oxz_nexus_c53300fc n/1 "),
            "expected dot-trimmed and UUID-collapsed hostname, got \
             {header:?}",
        );
    }

    #[test]
    fn hostname_none_drops_the_field_and_its_separator() {
        // With `None`, no hostname column exists: name/pid follows the
        // timestamp directly, separated by exactly one space (no
        // double-space artifact from a stripped placeholder).
        let e = parse(
            r#"{
                "v": 0,
                "level": 50,
                "name": "Nexus",
                "hostname": "ivanova",
                "pid": 100,
                "time": "2026-05-07T00:00:00Z",
                "msg": "kaboom"
            }"#,
        );
        let header =
            &format_event(&e, &opts(false, true, HostnameDisplay::None))[0];
        assert_eq!(header, "2026-05-07T00:00:00.000Z Nexus/100 ERROR kaboom",);
    }

    #[test]
    fn show_pid_false_drops_pid_and_slash() {
        // Default has pid hidden: the header should read just `<name>`
        // followed by a single space — no `/`, no pid digits.
        let e = parse(
            r#"{
                "v": 0,
                "level": 50,
                "name": "Nexus",
                "hostname": "host-a",
                "pid": 100,
                "time": "2026-05-07T00:00:00Z",
                "msg": "kaboom"
            }"#,
        );
        let header = &format_event(&e, &RenderOpts::default())[0];
        assert_eq!(
            header,
            "2026-05-07T00:00:00.000Z host-a Nexus ERROR kaboom",
        );
    }

    #[test]
    fn show_name_false_keeps_pid_alone() {
        // Hide name but show pid: the header should carry the pid as
        // its own column with no preceding `/`.
        let e = parse(
            r#"{
                "v": 0,
                "level": 30,
                "name": "Nexus",
                "hostname": "host-a",
                "pid": 42,
                "time": "2026-05-07T00:00:00Z",
                "msg": "m"
            }"#,
        );
        let opts = RenderOpts {
            show_pid: true,
            show_name: false,
            ..RenderOpts::default()
        };
        let header = &format_event(&e, &opts)[0];
        assert_eq!(header, "2026-05-07T00:00:00.000Z host-a 42 INFO  m",);
    }

    #[test]
    fn show_name_and_pid_both_false_drops_the_column() {
        // With both off, no name/pid column appears: hostname is
        // followed directly by the level column with exactly one
        // space between them (no double-space artifact).
        let e = parse(
            r#"{
                "v": 0,
                "level": 30,
                "name": "Nexus",
                "hostname": "host-a",
                "pid": 42,
                "time": "2026-05-07T00:00:00Z",
                "msg": "m"
            }"#,
        );
        let opts = RenderOpts {
            show_pid: false,
            show_name: false,
            ..RenderOpts::default()
        };
        let header = &format_event(&e, &opts)[0];
        assert_eq!(header, "2026-05-07T00:00:00.000Z host-a INFO  m");
    }

    #[test]
    fn level_column_is_left_aligned_to_five_columns() {
        // INFO/WARN are 4 chars and must pad to 5; ERROR/FATAL/TRACE/
        // DEBUG are already 5.  Pin the layout so callers downstream
        // (highlighting, search) can rely on consistent column offsets.
        let info = parse(
            r#"{"v":0,"level":30,"name":"n","hostname":"h","pid":1,
                "time":"2026-05-07T00:00:00Z","msg":"m"}"#,
        );
        let warn = parse(
            r#"{"v":0,"level":40,"name":"n","hostname":"h","pid":1,
                "time":"2026-05-07T00:00:00Z","msg":"m"}"#,
        );
        let error = parse(
            r#"{"v":0,"level":50,"name":"n","hostname":"h","pid":1,
                "time":"2026-05-07T00:00:00Z","msg":"m"}"#,
        );
        let info_line =
            &format_event(&info, &opts(false, true, HostnameDisplay::Short))[0];
        let warn_line =
            &format_event(&warn, &opts(false, true, HostnameDisplay::Short))[0];
        let error_line =
            &format_event(&error, &opts(false, true, HostnameDisplay::Short))
                [0];
        // The two characters following the level are always "  m" for
        // 4-char levels (padded plus space plus msg) and " m" for the
        // 5-char ones.
        assert!(info_line.contains(" INFO  m"));
        assert!(warn_line.contains(" WARN  m"));
        assert!(error_line.contains(" ERROR m"));
    }

    #[test]
    fn non_string_extras_keep_their_json_type() {
        let e = parse(
            r#"{
                "v": 0,
                "level": 30,
                "name": "Nexus",
                "hostname": "h",
                "pid": 1,
                "time": "2026-05-07T00:00:00Z",
                "msg": "m",
                "count": 42,
                "enabled": true,
                "ratio": 0.5,
                "tags": ["a", "b"],
                "meta": {"k": "v"},
                "absent": null
            }"#,
        );
        let lines = format_event(&e, &opts(true, true, HostnameDisplay::Short));
        // Header + 6 extras, sorted: absent, count, enabled, meta, ratio, tags.
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[1], "    absent = null");
        assert_eq!(lines[2], "    count = 42");
        assert_eq!(lines[3], "    enabled = true");
        assert_eq!(lines[4], r#"    meta = {"k":"v"}"#);
        assert_eq!(lines[5], "    ratio = 0.5");
        assert_eq!(lines[6], r#"    tags = ["a","b"]"#);
    }

    #[test]
    fn format_time_with_and_without_date() {
        let time: DateTime<Utc> =
            "2026-04-30T15:30:00.743162222Z".parse().unwrap();
        assert_eq!(
            format_time(&time, /* show_date = */ true),
            "2026-04-30T15:30:00.743Z",
        );
        assert_eq!(
            format_time(&time, /* show_date = */ false),
            "15:30:00.743Z",
        );
    }

    #[test]
    fn short_hostname_passes_plain_names_through() {
        // Single-component, no UUID suffix: no work to do.
        assert_eq!(short_hostname("ivanova"), "ivanova");
        assert_eq!(short_hostname("gimlet-01"), "gimlet-01");
        assert_eq!(short_hostname(""), "");
    }

    #[test]
    fn short_hostname_trims_dot_components() {
        assert_eq!(short_hostname("gimlet-01.oxide.test"), "gimlet-01",);
        // Multiple dots: only the first component survives, the rest
        // are domain-like suffixes.
        assert_eq!(short_hostname("a.b.c.d"), "a",);
    }

    #[test]
    fn short_hostname_collapses_uuid_suffix() {
        // Canonical example from the spec.
        assert_eq!(
            short_hostname("oxz_nexus_c53300fc-84eb-490a-9e1e-9e18d372856d",),
            "oxz_nexus_c53300fc",
        );
        // Bare UUID: collapses to its first 8-hex group.
        assert_eq!(
            short_hostname("c53300fc-84eb-490a-9e1e-9e18d372856d"),
            "c53300fc",
        );
        // Uppercase hex is recognized too.
        assert_eq!(
            short_hostname("svc_C53300FC-84EB-490A-9E1E-9E18D372856D"),
            "svc_C53300FC",
        );
    }

    #[test]
    fn short_hostname_combines_dot_trim_then_uuid_collapse() {
        // Real Oxide hostname: dotted *and* UUID-suffixed.  The dotted
        // trim runs first, so the UUID collapse only ever sees the
        // first component.
        assert_eq!(
            short_hostname(
                "oxz_nexus_c53300fc-84eb-490a-9e1e-9e18d372856d.oxide.test",
            ),
            "oxz_nexus_c53300fc",
        );
    }

    #[test]
    fn short_hostname_ignores_lookalike_suffixes() {
        // Trailing 4-4-4-12 hex with no preceding 8-hex boundary is
        // not a UUID — leave the hostname alone rather than chopping
        // mid-token.
        assert_eq!(
            short_hostname("not-uuid-1234-5678-90ab-cdef01234567"),
            "not-uuid-1234-5678-90ab-cdef01234567",
        );
        // A non-hex character inside what would otherwise be the
        // trailing 12-char group disqualifies the match.
        assert_eq!(
            short_hostname("svc_c53300fc-84eb-490a-9e1e-9e18d372856z"),
            "svc_c53300fc-84eb-490a-9e1e-9e18d372856z",
        );
    }
}
