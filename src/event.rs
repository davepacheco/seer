// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-memory representation of a single log record.
//!
//! Today this models a bunyan record only.  Other formats (CockroachDB,
//! syslog, raw plaintext) will be slotted in alongside as they're added.

use chrono::{DateTime, Utc};
use derive_more::{AsRef, Display, From};
use serde::Deserialize;
use serde::de::{IgnoredAny, MapAccess, Visitor};
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt;

/// A single log record.
///
/// Field set tracks the bunyan format: a fixed core (time, level, name,
/// hostname, pid, msg, v) plus arbitrary additional structured fields
/// captured in `extra`.
///
/// Deserialization tolerates duplicate keys: the first occurrence of
/// each key wins and subsequent duplicates are silently dropped.
/// Real-world Oxide bunyan logs occasionally repeat keys when nested
/// slog scopes both attach a context value (e.g. two layers each
/// setting `component`); failing the whole record over that loses more
/// information than it preserves.
#[derive(Debug, Clone)]
pub struct Event {
    pub time: DateTime<Utc>,
    pub level: Level,
    pub name: LoggerName,
    pub hostname: Hostname,
    pub pid: Pid,
    pub msg: String,
    /// bunyan record-format version
    pub v: u32,
    /// any additional structured fields beyond the bunyan core
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Self, D::Error> {
        struct EventVisitor;

        impl<'de> Visitor<'de> for EventVisitor {
            type Value = Event;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a bunyan log record")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Event, A::Error> {
                let mut time: Option<DateTime<Utc>> = None;
                let mut level: Option<Level> = None;
                let mut name: Option<LoggerName> = None;
                let mut hostname: Option<Hostname> = None;
                let mut pid: Option<Pid> = None;
                let mut msg: Option<String> = None;
                let mut v: Option<u32> = None;
                let mut extra: BTreeMap<String, serde_json::Value> =
                    BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    // First-wins for every key, core or extra.  Each
                    // arm must consume `next_value` exactly once so the
                    // map iterator advances; duplicates use
                    // `IgnoredAny` to drain the value without parsing.
                    macro_rules! take_first {
                        ($slot:ident) => {{
                            if $slot.is_some() {
                                let _: IgnoredAny = map.next_value()?;
                            } else {
                                $slot = Some(map.next_value()?);
                            }
                        }};
                    }
                    match key.as_str() {
                        "time" => take_first!(time),
                        "level" => take_first!(level),
                        "name" => take_first!(name),
                        "hostname" => take_first!(hostname),
                        "pid" => take_first!(pid),
                        "msg" => take_first!(msg),
                        "v" => take_first!(v),
                        _ => match extra.entry(key) {
                            Entry::Vacant(e) => {
                                e.insert(map.next_value()?);
                            }
                            Entry::Occupied(_) => {
                                let _: IgnoredAny = map.next_value()?;
                            }
                        },
                    }
                }
                Ok(Event {
                    time: time.ok_or_else(|| {
                        serde::de::Error::missing_field("time")
                    })?,
                    level: level.ok_or_else(|| {
                        serde::de::Error::missing_field("level")
                    })?,
                    name: name.ok_or_else(|| {
                        serde::de::Error::missing_field("name")
                    })?,
                    hostname: hostname.ok_or_else(|| {
                        serde::de::Error::missing_field("hostname")
                    })?,
                    pid: pid.ok_or_else(|| {
                        serde::de::Error::missing_field("pid")
                    })?,
                    msg: msg.ok_or_else(|| {
                        serde::de::Error::missing_field("msg")
                    })?,
                    v: v.ok_or_else(|| serde::de::Error::missing_field("v"))?,
                    extra,
                })
            }
        }

        deserializer.deserialize_map(EventVisitor)
    }
}

/// Bunyan log level.
///
/// Variants are ordered from least to most severe so derived `Ord` matches
/// severity (e.g. `Level::Warn > Level::Info`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Level {
    /// Returns the numeric value used by the bunyan format.
    pub fn as_bunyan_number(self) -> u8 {
        match self {
            Self::Trace => 10,
            Self::Debug => 20,
            Self::Info => 30,
            Self::Warn => 40,
            Self::Error => 50,
            Self::Fatal => 60,
        }
    }

    /// Returns the short uppercase name (e.g. `"INFO"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
            Self::Fatal => "FATAL",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Use `pad` (rather than `write_str`) so callers can rely on
        // width and alignment specifiers to align the names — `INFO`
        // and `WARN` are 4 chars while the others are 5, and the
        // log-line renderer wants a fixed-width column.
        f.pad(self.as_str())
    }
}

/// Error returned when a numeric value does not correspond to a bunyan
/// log level.
#[derive(Debug, thiserror::Error)]
#[error("unknown bunyan log level: {0}")]
pub struct UnknownLevel(pub u8);

impl TryFrom<u8> for Level {
    type Error = UnknownLevel;

    fn try_from(value: u8) -> Result<Self, UnknownLevel> {
        match value {
            10 => Ok(Self::Trace),
            20 => Ok(Self::Debug),
            30 => Ok(Self::Info),
            40 => Ok(Self::Warn),
            50 => Ok(Self::Error),
            60 => Ok(Self::Fatal),
            n => Err(UnknownLevel(n)),
        }
    }
}

impl<'de> Deserialize<'de> for Level {
    fn deserialize<D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Self, D::Error> {
        let n = u8::deserialize(d)?;
        Self::try_from(n).map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for Level {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(self.as_bunyan_number())
    }
}

// Manual JsonSchema impl: Level serializes as a bunyan-numeric u8
// (10, 20, 30, 40, 50, 60), not as the enum variant names.  The
// schema therefore advertises an integer with that enumerated set
// of allowed values rather than a string.
impl schemars::JsonSchema for Level {
    fn schema_name() -> String {
        "Level".to_owned()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("seer::event::Level")
    }

    fn json_schema(
        _: &mut schemars::r#gen::SchemaGenerator,
    ) -> schemars::schema::Schema {
        schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::Integer.into()),
            enum_values: Some(vec![
                serde_json::json!(10),
                serde_json::json!(20),
                serde_json::json!(30),
                serde_json::json!(40),
                serde_json::json!(50),
                serde_json::json!(60),
            ]),
            ..Default::default()
        }
        .into()
    }
}

/// Logger name (the `name` field in a bunyan record).
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Deserialize, Display, From, AsRef,
)]
#[serde(transparent)]
#[as_ref(forward)]
pub struct LoggerName(String);

/// Hostname recorded in a bunyan record.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Deserialize, Display, From, AsRef,
)]
#[serde(transparent)]
#[as_ref(forward)]
pub struct Hostname(String);

/// Process id from a bunyan record.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Display, From,
)]
#[serde(transparent)]
pub struct Pid(u32);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{TestDir, append_bunyan};
    use slog::info;

    #[test]
    fn level_round_trip_numbers() {
        for level in [
            Level::Trace,
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
            Level::Fatal,
        ] {
            let n = level.as_bunyan_number();
            let parsed = Level::try_from(n).expect("known level");
            assert_eq!(level, parsed);
        }
    }

    #[test]
    fn level_ord_matches_severity() {
        assert!(Level::Trace < Level::Debug);
        assert!(Level::Debug < Level::Info);
        assert!(Level::Info < Level::Warn);
        assert!(Level::Warn < Level::Error);
        assert!(Level::Error < Level::Fatal);
    }

    #[test]
    fn level_unknown_number_rejected() {
        let err = Level::try_from(99).unwrap_err();
        assert_eq!(err.0, 99);
    }

    #[test]
    fn parse_bunyan_record_emitted_by_slog() {
        let dir = TestDir::new();
        let p = dir.path().join("a.log");
        append_bunyan(&p, "myapp", |log| {
            info!(log, "hello world"; "component" => "Nexus");
        });
        let content = std::fs::read_to_string(&p).unwrap();
        let line = content
            .lines()
            .next()
            .expect("slog-bunyan wrote at least one line");

        let event: Event = serde_json::from_str(line).unwrap();
        assert_eq!(event.level, Level::Info);
        assert_eq!(event.name.to_string(), "myapp");
        assert_eq!(event.msg, "hello world");
        assert_eq!(event.v, 0);
        assert_eq!(
            event.extra.get("component").and_then(|v| v.as_str()),
            Some("Nexus")
        );
        // hostname and pid come from the running process; assert only
        // that the fields parsed.
        assert!(!AsRef::<str>::as_ref(&event.hostname).is_empty());
        let _: Pid = event.pid;

        dir.cleanup();
    }

    #[test]
    fn duplicate_keys_keep_first_occurrence() {
        // Real Oxide bunyan logs sometimes repeat keys when nested slog
        // scopes each attach a value (e.g. two layers setting
        // `component`); we keep the first and drop the rest rather
        // than failing the line.
        let line = r#"{"msg":"hello","v":0,"name":"first","level":30,"time":"2026-04-30T19:49:19Z","hostname":"h","pid":42,"component":"datastore","component":"nexus","name":"second"}"#;
        let event: Event = serde_json::from_str(line).unwrap();
        // First `name` wins over the late-appearing duplicate.
        assert_eq!(event.name.to_string(), "first");
        // First extra-field occurrence wins too.
        assert_eq!(
            event.extra.get("component").and_then(|v| v.as_str()),
            Some("datastore"),
        );
    }
}
