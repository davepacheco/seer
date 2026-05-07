// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Filter expressions over [`Event`]s.
//!
//! A [`Filter`] is a list of [`Predicate`]s combined by AND.  An empty
//! filter accepts every event.
//!
//! # String form
//!
//! Filters round-trip through a small whitespace-separated DSL:
//!
//! ```text
//! level>=warn name=Nexus msg=~"oh no"
//! ```
//!
//! Tokens are split with [`shlex`], so values containing spaces can be
//! double-quoted.  Each token is one predicate; the supported operators
//! are `>=` (level only), `==` and `=` (equality), `!=` (negated
//! equality), `=~` (regex, today only on `msg`), and `!~` (negated
//! regex, msg only).  Level names are case-insensitive.
//!
//! Both [`Filter`] and [`Predicate`] are also `serde`-serializable so
//! they can ride along in a persisted session; the regex's source string
//! is what's stored.

use crate::event::{Event, Level};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

/// A conjunction of predicates over an [`Event`].
///
/// The default value has no predicates and matches every event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Filter {
    predicates: Vec<Predicate>,
}

impl Filter {
    /// Returns true iff every predicate matches `event`.
    pub fn matches(&self, event: &Event) -> bool {
        self.predicates.iter().all(|p| p.matches(event))
    }

    /// Returns the predicates this filter is built from.
    pub fn predicates(&self) -> &[Predicate] {
        &self.predicates
    }
}

/// A single predicate over an [`Event`].
///
/// The equality and regex variants carry a `negated` flag so that
/// `name=Nexus` and `name!=Nexus` (and likewise `msg=~foo` / `msg!~foo`)
/// are the same variant differing only in polarity.  `LevelAtLeast` has
/// no useful negation (the user can write `level>=` at the threshold
/// they want), so it is left as a single variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Predicate {
    /// `event.level >= threshold`
    LevelAtLeast(Level),
    /// `event.level == level` (or `!=` when `negated`)
    LevelEquals {
        level: Level,
        #[serde(default)]
        negated: bool,
    },
    /// Named field has (or, when `negated`, does not have) the given
    /// exact-string value.
    ///
    /// Matches the bunyan core fields (`name`, `hostname`, `pid`, `msg`)
    /// or any key in `event.extra`.
    FieldEquals {
        name: String,
        value: String,
        #[serde(default)]
        negated: bool,
    },
    /// `event.msg` matches (or, when `negated`, does not match) the regex.
    MsgMatches {
        #[serde(with = "regex_serde")]
        regex: Regex,
        #[serde(default)]
        negated: bool,
    },
}

impl Predicate {
    pub fn matches(&self, event: &Event) -> bool {
        match self {
            Self::LevelAtLeast(threshold) => event.level >= *threshold,
            Self::LevelEquals { level, negated } => {
                (event.level == *level) ^ *negated
            }
            Self::FieldEquals { name, value, negated } => {
                field_matches(event, name, value) ^ *negated
            }
            Self::MsgMatches { regex, negated } => {
                regex.is_match(&event.msg) ^ *negated
            }
        }
    }
}

fn field_matches(event: &Event, name: &str, value: &str) -> bool {
    match name {
        "name" => event.name.to_string() == value,
        "hostname" => event.hostname.to_string() == value,
        "pid" => event.pid.to_string() == value,
        "msg" => event.msg == value,
        // Bunyan version isn't usually filtered, but it's a core field
        // so handle it consistently.
        "v" => event.v.to_string() == value,
        // Anything else is in `extra`.  We compare against string-typed
        // values directly via serde_json's PartialEq<&str> for Value;
        // null gets a literal `"null"` shorthand.  Bool/Number values
        // in `extra` won't match yet — extending the predicate language
        // to reach them is a separate item.
        other => match event.extra.get(other) {
            Some(serde_json::Value::Null) => value == "null",
            Some(v) => *v == value,
            None => false,
        },
    }
}

/// Failure to parse a filter expression.
#[derive(Debug, thiserror::Error)]
pub enum FilterParseError {
    #[error("could not tokenize filter (unbalanced quotes?)")]
    Tokenize,
    #[error("token {token:?} has no recognized operator")]
    NoOperator { token: String },
    #[error("empty field name in token {token:?}")]
    EmptyName { token: String },
    #[error(
        "operator {op:?} is not supported for field {name:?} \
         (today only `msg=~regex` is allowed)"
    )]
    UnsupportedFieldOp { name: String, op: String },
    #[error("unknown level {0:?} (expected trace/debug/info/warn/error/fatal)")]
    BadLevel(String),
    #[error("invalid regex: {0}")]
    BadRegex(String),
}

impl FromStr for Filter {
    type Err = FilterParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let tokens = shlex::split(s).ok_or(FilterParseError::Tokenize)?;
        let predicates = tokens
            .iter()
            .map(|t| parse_predicate(t))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Filter { predicates })
    }
}

fn parse_predicate(tok: &str) -> Result<Predicate, FilterParseError> {
    // Order matters: multi-char operators that overlap with shorter ones
    // must be probed first.  `!=` and `!~` come before `=`; `=~` and
    // `==` and `>=` likewise.
    if let Some((lhs, rhs)) = tok.split_once("=~") {
        return parse_msg_regex(tok, lhs, rhs, /* negated = */ false);
    }
    if let Some((lhs, rhs)) = tok.split_once("!~") {
        return parse_msg_regex(tok, lhs, rhs, /* negated = */ true);
    }
    if let Some((lhs, rhs)) = tok.split_once(">=") {
        require_nonempty_name(tok, lhs)?;
        return if lhs == "level" {
            Ok(Predicate::LevelAtLeast(parse_level(rhs)?))
        } else {
            Err(FilterParseError::UnsupportedFieldOp {
                name: lhs.to_string(),
                op: ">=".to_string(),
            })
        };
    }
    if let Some((lhs, rhs)) = tok.split_once("==") {
        return field_or_level(tok, lhs, rhs, /* negated = */ false);
    }
    if let Some((lhs, rhs)) = tok.split_once("!=") {
        return field_or_level(tok, lhs, rhs, /* negated = */ true);
    }
    if let Some((lhs, rhs)) = tok.split_once('=') {
        return field_or_level(tok, lhs, rhs, /* negated = */ false);
    }
    Err(FilterParseError::NoOperator { token: tok.to_string() })
}

fn parse_msg_regex(
    tok: &str,
    lhs: &str,
    rhs: &str,
    negated: bool,
) -> Result<Predicate, FilterParseError> {
    require_nonempty_name(tok, lhs)?;
    if lhs != "msg" {
        return Err(FilterParseError::UnsupportedFieldOp {
            name: lhs.to_string(),
            op: if negated { "!~" } else { "=~" }.to_string(),
        });
    }
    let regex = Regex::new(rhs)
        .map_err(|e| FilterParseError::BadRegex(e.to_string()))?;
    Ok(Predicate::MsgMatches { regex, negated })
}

fn field_or_level(
    tok: &str,
    lhs: &str,
    rhs: &str,
    negated: bool,
) -> Result<Predicate, FilterParseError> {
    require_nonempty_name(tok, lhs)?;
    if lhs == "level" {
        Ok(Predicate::LevelEquals { level: parse_level(rhs)?, negated })
    } else {
        Ok(Predicate::FieldEquals {
            name: lhs.to_string(),
            value: rhs.to_string(),
            negated,
        })
    }
}

fn require_nonempty_name(
    tok: &str,
    lhs: &str,
) -> Result<(), FilterParseError> {
    if lhs.is_empty() {
        Err(FilterParseError::EmptyName { token: tok.to_string() })
    } else {
        Ok(())
    }
}

fn parse_level(s: &str) -> Result<Level, FilterParseError> {
    match s.to_ascii_lowercase().as_str() {
        "trace" => Ok(Level::Trace),
        "debug" => Ok(Level::Debug),
        "info" => Ok(Level::Info),
        "warn" => Ok(Level::Warn),
        "error" => Ok(Level::Error),
        "fatal" => Ok(Level::Fatal),
        _ => Err(FilterParseError::BadLevel(s.to_string())),
    }
}

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for p in &self.predicates {
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            write!(f, "{p}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LevelAtLeast(l) => {
                write!(f, "level>={}", level_token(*l))
            }
            Self::LevelEquals { level, negated } => {
                let op = if *negated { "!=" } else { "=" };
                write!(f, "level{}{}", op, level_token(*level))
            }
            Self::FieldEquals { name, value, negated } => {
                let op = if *negated { "!=" } else { "=" };
                write!(f, "{}{}{}", name, op, quote(value))
            }
            Self::MsgMatches { regex, negated } => {
                let op = if *negated { "!~" } else { "=~" };
                write!(f, "msg{}{}", op, quote(regex.as_str()))
            }
        }
    }
}

fn level_token(l: Level) -> &'static str {
    match l {
        Level::Trace => "trace",
        Level::Debug => "debug",
        Level::Info => "info",
        Level::Warn => "warn",
        Level::Error => "error",
        Level::Fatal => "fatal",
    }
}

fn quote(s: &str) -> Cow<'_, str> {
    // shlex's own quote is conservative for shell safety (it would
    // wrap `foo.*` in quotes because of `*`).  Our DSL is parsed by
    // shlex but not by a shell, so we only need to quote when the
    // value contains whitespace or shlex-significant characters
    // (quotes / backslash).  Keeps the common case unadorned.
    let needs_quote = s.is_empty()
        || s.chars().any(|c| {
            c.is_whitespace() || c == '"' || c == '\'' || c == '\\'
        });
    if !needs_quote {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Cow::Owned(out)
}

mod regex_serde {
    use regex::Regex;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        r: &Regex,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.serialize_str(r.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Regex, D::Error> {
        let s = String::deserialize(d)?;
        Regex::new(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an [`Event`] from a JSON snippet.  We use hand-crafted
    /// JSON here rather than driving slog-bunyan because predicate
    /// tests need precise control over fields like `hostname` and
    /// `pid` that slog-bunyan derives from the running process.
    fn ev(json: &str) -> Event {
        serde_json::from_str(json).expect("test event JSON")
    }

    fn base_event() -> Event {
        ev(r#"{
            "v": 0,
            "level": 30,
            "name": "Nexus",
            "hostname": "sled-01",
            "pid": 1234,
            "time": "2025-04-01T00:00:00Z",
            "msg": "blueprint executed",
            "component": "nexus"
        }"#)
    }

    // ---------- predicates ----------

    #[test]
    fn level_at_least_matches_above_and_equal() {
        let e = base_event(); // info
        assert!(Predicate::LevelAtLeast(Level::Info).matches(&e));
        assert!(Predicate::LevelAtLeast(Level::Debug).matches(&e));
        assert!(!Predicate::LevelAtLeast(Level::Warn).matches(&e));
    }

    #[test]
    fn level_equals_only_exact() {
        let e = base_event(); // info
        assert!(
            Predicate::LevelEquals { level: Level::Info, negated: false }
                .matches(&e)
        );
        assert!(
            !Predicate::LevelEquals { level: Level::Warn, negated: false }
                .matches(&e)
        );
    }

    #[test]
    fn field_equals_core_fields() {
        let e = base_event();
        assert!(Predicate::FieldEquals {
            name: "name".into(),
            value: "Nexus".into(),
            negated: false,
        }
        .matches(&e));
        assert!(!Predicate::FieldEquals {
            name: "name".into(),
            value: "SledAgent".into(),
            negated: false,
        }
        .matches(&e));
        assert!(Predicate::FieldEquals {
            name: "hostname".into(),
            value: "sled-01".into(),
            negated: false,
        }
        .matches(&e));
        assert!(Predicate::FieldEquals {
            name: "pid".into(),
            value: "1234".into(),
            negated: false,
        }
        .matches(&e));
    }

    #[test]
    fn field_equals_extra_field() {
        let e = base_event();
        assert!(Predicate::FieldEquals {
            name: "component".into(),
            value: "nexus".into(),
            negated: false,
        }
        .matches(&e));
        assert!(!Predicate::FieldEquals {
            name: "component".into(),
            value: "sled-agent".into(),
            negated: false,
        }
        .matches(&e));
    }

    #[test]
    fn field_equals_missing_field_does_not_match() {
        let e = base_event();
        assert!(!Predicate::FieldEquals {
            name: "nope".into(),
            value: "anything".into(),
            negated: false,
        }
        .matches(&e));
    }

    #[test]
    fn msg_matches_regex() {
        let e = base_event();
        assert!(
            Predicate::MsgMatches {
                regex: Regex::new("blueprint").unwrap(),
                negated: false,
            }
            .matches(&e)
        );
        assert!(
            Predicate::MsgMatches {
                regex: Regex::new("^blueprint exec").unwrap(),
                negated: false,
            }
            .matches(&e)
        );
        assert!(
            !Predicate::MsgMatches {
                regex: Regex::new("nope").unwrap(),
                negated: false,
            }
            .matches(&e)
        );
    }

    #[test]
    fn empty_filter_matches_everything() {
        let e = base_event();
        assert!(Filter::default().matches(&e));
    }

    #[test]
    fn filter_is_conjunction() {
        let e = base_event();
        let f = Filter {
            predicates: vec![
                Predicate::LevelAtLeast(Level::Info),
                Predicate::FieldEquals {
                    name: "name".into(),
                    value: "Nexus".into(),
                    negated: false,
                },
            ],
        };
        assert!(f.matches(&e));

        let f = Filter {
            predicates: vec![
                Predicate::LevelAtLeast(Level::Info),
                Predicate::FieldEquals {
                    name: "name".into(),
                    value: "Other".into(),
                    negated: false,
                },
            ],
        };
        assert!(!f.matches(&e));
    }

    // ---------- parser ----------

    fn parse(s: &str) -> Filter {
        s.parse().unwrap_or_else(|e| panic!("parsing {s:?}: {e}"))
    }

    #[test]
    fn parse_empty_string_is_empty_filter() {
        let f: Filter = "".parse().unwrap();
        assert!(f.predicates().is_empty());
    }

    #[test]
    fn parse_whitespace_is_empty_filter() {
        let f: Filter = "   \t  ".parse().unwrap();
        assert!(f.predicates().is_empty());
    }

    #[test]
    fn parse_level_at_least() {
        let f = parse("level>=warn");
        assert!(matches!(
            f.predicates()[0],
            Predicate::LevelAtLeast(Level::Warn)
        ));
    }

    #[test]
    fn parse_level_equals_single_and_double_eq() {
        for src in ["level=error", "level==error"] {
            let f = parse(src);
            assert!(matches!(
                f.predicates()[0],
                Predicate::LevelEquals {
                    level: Level::Error,
                    negated: false
                },
            ));
        }
    }

    #[test]
    fn parse_level_name_is_case_insensitive() {
        for src in ["level>=WARN", "level>=Warn", "level>=warn"] {
            let f = parse(src);
            assert!(matches!(
                f.predicates()[0],
                Predicate::LevelAtLeast(Level::Warn)
            ));
        }
    }

    #[test]
    fn parse_field_equals() {
        let f = parse("name=Nexus");
        match &f.predicates()[0] {
            Predicate::FieldEquals { name, value, negated } => {
                assert_eq!(name, "name");
                assert_eq!(value, "Nexus");
                assert!(!negated);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_msg_regex() {
        let f = parse("msg=~oh.*no");
        match &f.predicates()[0] {
            Predicate::MsgMatches { regex, negated } => {
                assert_eq!(regex.as_str(), "oh.*no");
                assert!(!negated);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_quoted_value_preserves_spaces() {
        let f = parse(r#"msg=~"oh no""#);
        match &f.predicates()[0] {
            Predicate::MsgMatches { regex, negated } => {
                assert_eq!(regex.as_str(), "oh no");
                assert!(!negated);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_multiple_predicates_are_anded() {
        let f = parse("level>=warn name=Nexus msg=~boom");
        assert_eq!(f.predicates().len(), 3);
    }

    // ---------- negation ----------

    #[test]
    fn level_equals_negated_inverts() {
        let e = base_event(); // info
        assert!(
            !Predicate::LevelEquals { level: Level::Info, negated: true }
                .matches(&e)
        );
        assert!(
            Predicate::LevelEquals { level: Level::Warn, negated: true }
                .matches(&e)
        );
    }

    #[test]
    fn field_equals_negated_inverts() {
        let e = base_event();
        assert!(!Predicate::FieldEquals {
            name: "name".into(),
            value: "Nexus".into(),
            negated: true,
        }
        .matches(&e));
        assert!(Predicate::FieldEquals {
            name: "name".into(),
            value: "Other".into(),
            negated: true,
        }
        .matches(&e));
    }

    #[test]
    fn field_equals_negated_missing_field_matches() {
        // `field_matches` is false when the field is missing; negated,
        // that becomes true.  This is the natural reading of "every
        // event without an `nope` field" but worth pinning down with a
        // test since it's a behavior that's easy to flip accidentally.
        let e = base_event();
        assert!(Predicate::FieldEquals {
            name: "nope".into(),
            value: "anything".into(),
            negated: true,
        }
        .matches(&e));
    }

    #[test]
    fn msg_matches_negated_inverts() {
        let e = base_event();
        assert!(!Predicate::MsgMatches {
            regex: Regex::new("blueprint").unwrap(),
            negated: true,
        }
        .matches(&e));
        assert!(Predicate::MsgMatches {
            regex: Regex::new("nope").unwrap(),
            negated: true,
        }
        .matches(&e));
    }

    #[test]
    fn parse_field_not_equals() {
        let f = parse("name!=Nexus");
        match &f.predicates()[0] {
            Predicate::FieldEquals { name, value, negated } => {
                assert_eq!(name, "name");
                assert_eq!(value, "Nexus");
                assert!(negated);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_level_not_equals() {
        let f = parse("level!=warn");
        match &f.predicates()[0] {
            Predicate::LevelEquals { level, negated } => {
                assert_eq!(*level, Level::Warn);
                assert!(negated);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_msg_not_matches() {
        let f = parse("msg!~oh.*no");
        match &f.predicates()[0] {
            Predicate::MsgMatches { regex, negated } => {
                assert_eq!(regex.as_str(), "oh.*no");
                assert!(negated);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_unsupported_negated_regex_field_errors() {
        let err = "name!~foo".parse::<Filter>().unwrap_err();
        assert!(matches!(
            err,
            FilterParseError::UnsupportedFieldOp { ref name, ref op }
                if name == "name" && op == "!~"
        ));
    }

    #[test]
    fn display_negated_forms() {
        let f = Filter {
            predicates: vec![
                Predicate::LevelEquals {
                    level: Level::Error,
                    negated: true,
                },
                Predicate::FieldEquals {
                    name: "name".into(),
                    value: "Nexus".into(),
                    negated: true,
                },
                Predicate::MsgMatches {
                    regex: Regex::new("foo.*").unwrap(),
                    negated: true,
                },
            ],
        };
        assert_eq!(
            f.to_string(),
            "level!=error name!=Nexus msg!~foo.*",
        );
    }

    #[test]
    fn display_then_parse_round_trip_negated() {
        let inputs =
            ["level!=warn", "name!=Nexus", "msg!~foo.*bar"];
        for src in inputs {
            let parsed: Filter = src.parse().unwrap();
            let displayed = parsed.to_string();
            let reparsed: Filter = displayed.parse().unwrap();
            assert_eq!(
                displayed,
                reparsed.to_string(),
                "round-trip drifted for {src:?}",
            );
        }
    }

    #[test]
    fn serde_payload_without_negated_field_defaults_false() {
        // `#[serde(default)]` lets payloads omit `negated`; it reads as
        // false.  Useful so a future predicate-schema bump that adds
        // another flag can be made backward-compatible the same way.
        let json = r#"{
            "predicates": [
                { "FieldEquals": { "name": "name", "value": "Nexus" } },
                { "MsgMatches": { "regex": "boom" } },
                { "LevelEquals": { "level": 40 } }
            ]
        }"#;
        let f: Filter = serde_json::from_str(json).unwrap();
        match &f.predicates()[0] {
            Predicate::FieldEquals { negated, .. } => assert!(!negated),
            other => panic!("unexpected: {other:?}"),
        }
        match &f.predicates()[1] {
            Predicate::MsgMatches { negated, .. } => assert!(!negated),
            other => panic!("unexpected: {other:?}"),
        }
        match &f.predicates()[2] {
            Predicate::LevelEquals { negated, .. } => assert!(!negated),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---------- parser errors ----------

    #[test]
    fn parse_unknown_level_errors() {
        let err: FilterParseError = "level>=loud".parse::<Filter>().unwrap_err();
        assert!(matches!(err, FilterParseError::BadLevel(ref s) if s == "loud"));
    }

    #[test]
    fn parse_unsupported_regex_field_errors() {
        let err = "name=~foo".parse::<Filter>().unwrap_err();
        assert!(matches!(
            err,
            FilterParseError::UnsupportedFieldOp { ref name, ref op }
                if name == "name" && op == "=~"
        ));
    }

    #[test]
    fn parse_no_operator_errors() {
        let err = "loose".parse::<Filter>().unwrap_err();
        assert!(matches!(
            err,
            FilterParseError::NoOperator { ref token } if token == "loose"
        ));
    }

    #[test]
    fn parse_empty_name_errors() {
        let err = "=value".parse::<Filter>().unwrap_err();
        assert!(matches!(err, FilterParseError::EmptyName { .. }));
    }

    #[test]
    fn parse_bad_regex_errors() {
        let err = "msg=~(unclosed".parse::<Filter>().unwrap_err();
        assert!(matches!(err, FilterParseError::BadRegex(_)));
    }

    #[test]
    fn parse_unbalanced_quotes_errors() {
        let err = "name=\"unterminated".parse::<Filter>().unwrap_err();
        assert!(matches!(err, FilterParseError::Tokenize));
    }

    // ---------- display + round-trip ----------

    #[test]
    fn display_canonical_forms() {
        let f = Filter {
            predicates: vec![
                Predicate::LevelAtLeast(Level::Warn),
                Predicate::LevelEquals {
                    level: Level::Error,
                    negated: false,
                },
                Predicate::FieldEquals {
                    name: "name".into(),
                    value: "Nexus".into(),
                    negated: false,
                },
                Predicate::MsgMatches {
                    regex: Regex::new("foo.*").unwrap(),
                    negated: false,
                },
            ],
        };
        assert_eq!(
            f.to_string(),
            "level>=warn level=error name=Nexus msg=~foo.*",
        );
    }

    #[test]
    fn display_quotes_values_with_spaces() {
        let f = Filter {
            predicates: vec![Predicate::FieldEquals {
                name: "msg".into(),
                value: "oh no".into(),
                negated: false,
            }],
        };
        // Some shlex versions emit single quotes, some emit double; we
        // care that the round-trip works, not the exact byte sequence.
        let parsed: Filter = f.to_string().parse().unwrap();
        match &parsed.predicates()[0] {
            Predicate::FieldEquals { value, .. } => {
                assert_eq!(value, "oh no");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn display_then_parse_round_trip() {
        // Cover each predicate variant.
        let inputs = [
            "level>=warn",
            "level=error",
            "name=Nexus",
            "component=nexus",
            "msg=~foo.*bar",
            "level>=info name=Nexus msg=~boom",
        ];
        for src in inputs {
            let parsed: Filter = src.parse().unwrap();
            let displayed = parsed.to_string();
            let reparsed: Filter = displayed.parse().unwrap();
            assert_eq!(
                displayed,
                reparsed.to_string(),
                "round-trip drifted for {src:?}",
            );
        }
    }

    // ---------- serde ----------

    #[test]
    fn filter_round_trips_through_serde() {
        let f = Filter {
            predicates: vec![
                Predicate::LevelAtLeast(Level::Warn),
                Predicate::FieldEquals {
                    name: "name".into(),
                    value: "Nexus".into(),
                    negated: false,
                },
                Predicate::MsgMatches {
                    regex: Regex::new("boom").unwrap(),
                    negated: false,
                },
            ],
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: Filter = serde_json::from_str(&json).unwrap();
        assert_eq!(f.to_string(), back.to_string());
    }
}
