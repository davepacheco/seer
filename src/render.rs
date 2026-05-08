// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Formatting an [`Event`] into the lines that the CLI prints and the TUI
//! draws.
//!
//! The shape mirrors `looker --output long`: a header line built from the
//! bunyan core fields, followed by one indented `key = value` line per
//! additional structured field.  The differences from looker are
//! deliberate: full RFC 3339 timestamps (no millisecond truncation) and
//! no color, since both binaries emit either to a TTY-agnostic file or a
//! ratatui buffer that owns its own styling.
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

/// Formats `event` into one or more display lines.
///
/// The first line is the bunyan header
/// (`<time> <LEVEL> <name>/<pid> on <hostname>: <msg>`); when
/// `show_extras` is true, subsequent lines are `    <key> = <json-value>`,
/// one per entry in `event.extra`, ordered by key.  The returned vec is
/// non-empty.
pub fn format_event(event: &Event, show_extras: bool) -> Vec<String> {
    let cap = if show_extras { 1 + event.extra.len() } else { 1 };
    let mut lines = Vec::with_capacity(cap);
    // Level is padded to 5 columns (the width of the longest variant,
    // `TRACE`/`DEBUG`/`ERROR`/`FATAL`) so the column following it lines
    // up across rows of mixed severity.
    lines.push(format!(
        "{} {:<5} {}/{} on {}: {}",
        event.time.to_rfc3339(),
        event.level,
        event.name,
        event.pid,
        event.hostname,
        event.msg,
    ));
    if show_extras {
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
        let lines = format_event(&e, /* show_extras = */ true);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            "2026-05-07T04:48:12.142223551+00:00 INFO  \
             Nexus/15797 on ivanova: Nexus starting up",
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
        let lines = format_event(&e, /* show_extras = */ true);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with(": Nexus starting up"));
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
        let lines = format_event(&e, /* show_extras = */ false);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with(": Nexus starting up"));
    }

    #[test]
    fn header_format_matches_looker_layout() {
        // Asserts the exact layout users will compare against looker's
        // output: timestamp, then level, then name/pid on hostname:
        // msg.  Pin this so accidental refactors don't drift the format.
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
        let header = &format_event(&e, /* show_extras = */ true)[0];
        assert_eq!(
            header,
            "2026-05-07T00:00:00+00:00 ERROR Nexus/100 on host-a: kaboom",
        );
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
        // The two characters following the level are always "  " for
        // 4-char levels and " <name>" for 5-char ones.
        let info_line = &format_event(&info, false)[0];
        let warn_line = &format_event(&warn, false)[0];
        let error_line = &format_event(&error, false)[0];
        assert!(info_line.contains(" INFO  n/"));
        assert!(warn_line.contains(" WARN  n/"));
        assert!(error_line.contains(" ERROR n/"));
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
        let lines = format_event(&e, /* show_extras = */ true);
        // Header + 6 extras, sorted: absent, count, enabled, meta, ratio, tags.
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[1], "    absent = null");
        assert_eq!(lines[2], "    count = 42");
        assert_eq!(lines[3], "    enabled = true");
        assert_eq!(lines[4], r#"    meta = {"k":"v"}"#);
        assert_eq!(lines[5], "    ratio = 0.5");
        assert_eq!(lines[6], r#"    tags = ["a","b"]"#);
    }
}
