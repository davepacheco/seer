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
use std::collections::BTreeMap;
use std::fmt;

/// A single log record.
///
/// Field set tracks the bunyan format: a fixed core (time, level, name,
/// hostname, pid, msg, v) plus arbitrary additional structured fields
/// captured in `extra`.
#[derive(Debug, Clone, Deserialize)]
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
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Bunyan log level.
///
/// Variants are ordered from least to most severe so derived `Ord` matches
/// severity (e.g. `Level::Warn > Level::Info`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
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
        f.write_str(self.as_str())
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
    fn serialize<S: serde::Serializer>(
        &self,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.serialize_u8(self.as_bunyan_number())
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
    use crate::test_util::{TestDir, append_bunyan};
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
}
