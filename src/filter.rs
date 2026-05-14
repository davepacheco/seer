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
//! are `>=` (level or time), `<=`, `>`, `<` (time only), `==` and `=`
//! (equality), `!=` (negated equality), `=~` (regex, on `msg` and
//! `source_id`), and `!~` (negated regex, on the same).  Level names
//! are case-insensitive.  Time values are parsed as RFC 3339 (e.g.
//! `time>=2026-05-09T12:00:00Z`).
//!
//! `source_id=~regex` (and `!~`) is special: it filters whole sources
//! at query time, before any of their lines are read, rather than
//! per event.  Equality forms (`source_id=foo`) are not supported —
//! the parser rejects them rather than silently routing to a
//! never-matching field equality.
//!
//! Both [`Filter`] and [`Predicate`] are also `serde`-serializable so
//! they can ride along in a persisted session.

use crate::event::{Event, Level};
use crate::position::SourceId;
use chrono::{DateTime, SecondsFormat, Utc};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

/// A conjunction of predicates over an [`Event`].
///
/// The default value has no predicates and matches every event.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Filter {
    predicates: Vec<Predicate>,
}

impl Filter {
    /// Returns true iff every event predicate accepts `event`.
    ///
    /// [`SourcePredicate`]s are evaluated separately at source-selection
    /// time (see [`Self::matches_source_id`]); they don't constrain
    /// individual events and so contribute `true` to the per-event
    /// conjunction here.  The engine is responsible for filtering whole
    /// sources up front so this method is only ever called for events
    /// that already cleared the source-id check.
    pub fn matches_event(&self, event: &Event) -> bool {
        self.predicates.iter().all(|p| match p {
            Predicate::Event(ep) => ep.matches_event(event),
            Predicate::Source(_) => true,
        })
    }

    /// Returns true iff every source-id predicate accepts `source_id`.
    ///
    /// [`EventPredicate`]s don't constrain source ids, so they
    /// contribute `true` to the conjunction; a filter with no
    /// source-id predicates accepts every source.  The engine uses
    /// this to skip whole sources before constructing cursors over
    /// them — a filtered-out source is never queried for events.
    pub fn matches_source_id(&self, source_id: &SourceId) -> bool {
        self.predicates.iter().all(|p| match p {
            Predicate::Source(sp) => sp.matches_source_id(source_id),
            Predicate::Event(_) => true,
        })
    }

    /// Returns the predicates this filter is built from.
    pub fn predicates(&self) -> &[Predicate] {
        &self.predicates
    }

    /// Appends `predicate` to the conjunction.  Used by callers that
    /// build a filter incrementally (e.g. the TUI's exclude mode adds a
    /// `msg != <selected message>` predicate to the active filter).
    pub fn add_predicate(&mut self, predicate: Predicate) {
        self.predicates.push(predicate);
    }
}

/// Whether a predicate is satisfied when its underlying condition
/// holds (the affirmed form, e.g. `name=Nexus`) or when it doesn't
/// (the negated form, e.g. `name!=Nexus`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema,
)]
pub enum Form {
    /// The predicate accepts events for which the condition is true
    /// (e.g. `name=Nexus`, `msg=~"oh no"`).
    Affirmed,
    /// The predicate accepts events for which the condition is false
    /// (e.g. `name!=Nexus`, `msg!~"oh no"`).
    Negated,
}

impl Form {
    /// Combines a raw condition outcome with this form: `Affirmed`
    /// returns it as-is; `Negated` inverts it.
    pub fn applied_to(self, condition: bool) -> bool {
        match self {
            Form::Affirmed => condition,
            Form::Negated => !condition,
        }
    }
}

/// Name of a field that a predicate matches against.
///
/// Bunyan core fields are typed by [`CoreField`]; anything else lives
/// in the event's `extra` map and is carried as the raw key string.
/// The DSL parser converts a token's left-hand side via the
/// `From<&str>` impl, which routes the known core names into
/// [`Self::Core`] and everything else into [`Self::Extra`].
///
/// Serializes / deserializes as a bare DSL spelling string (`"name"`,
/// `"hostname"`, `"build"`, ...) via the
/// `#[serde(into = ..., from = ...)]` round-trip, so the on-disk
/// form mirrors what a user would type into the filter dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(into = "String", from = "String")]
#[schemars(with = "String")]
pub enum FieldName {
    Core(CoreField),
    Extra(String),
}

impl From<FieldName> for String {
    fn from(name: FieldName) -> String {
        name.to_string()
    }
}

impl fmt::Display for FieldName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FieldName::Core(c) => c.fmt(f),
            FieldName::Extra(name) => f.write_str(name),
        }
    }
}

impl From<&str> for FieldName {
    fn from(s: &str) -> Self {
        match s {
            "name" => FieldName::Core(CoreField::Name),
            "hostname" => FieldName::Core(CoreField::Hostname),
            "pid" => FieldName::Core(CoreField::Pid),
            "msg" => FieldName::Core(CoreField::Msg),
            other => FieldName::Extra(other.to_string()),
        }
    }
}

impl From<String> for FieldName {
    fn from(s: String) -> Self {
        // Going through `&str` keeps the core-name lookup in one
        // place; the bookkeeping cost of allocating a temporary
        // `String` for the `Extra` arm is negligible against the
        // rest of the parse path.
        FieldName::from(s.as_str())
    }
}

/// One of the bunyan log record's structured top-level fields.
///
/// Enumerated rather than carried as a string so that consumers can match
/// exhaustively.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum CoreField {
    Name,
    Hostname,
    Pid,
    Msg,
}

impl CoreField {
    /// Returns the DSL spelling for this core field.  Round-trips
    /// through [`FieldName::from`].
    pub fn as_str(self) -> &'static str {
        match self {
            CoreField::Name => "name",
            CoreField::Hostname => "hostname",
            CoreField::Pid => "pid",
            CoreField::Msg => "msg",
        }
    }
}

impl fmt::Display for CoreField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single predicate in a [`Filter`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum Predicate {
    Event(EventPredicate),
    Source(SourcePredicate),
}

impl From<EventPredicate> for Predicate {
    fn from(p: EventPredicate) -> Self {
        Self::Event(p)
    }
}

impl From<SourcePredicate> for Predicate {
    fn from(p: SourcePredicate) -> Self {
        Self::Source(p)
    }
}

/// A predicate evaluated against an individual [`Event`].
///
/// The equality and regex variants carry a [`Form`] so that
/// `name=Nexus` and `name!=Nexus` (and likewise `msg=~foo` / `msg!~foo`)
/// are the same variant differing only in form.  `LevelAtLeast` has
/// no useful negation (the user can write `level>=` at the threshold
/// they want), so it is left as a single variant.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum EventPredicate {
    /// `event.level >= threshold`
    LevelAtLeast(Level),
    /// `event.level == level` (or `!=` when `form` is `Negated`)
    LevelEquals { level: Level, form: Form },
    /// Named field has (or, when `form` is `Negated`, does not have)
    /// the given exact-string value.
    ///
    /// `name` is either a typed bunyan core field
    /// ([`FieldName::Core`]) or a free-form `extra` key
    /// ([`FieldName::Extra`]).
    FieldEquals { name: FieldName, value: String, form: Form },
    /// `event.msg` matches (or, when `form` is `Negated`, does not
    /// match) the regex.
    MsgMatches {
        #[serde(with = "regex_serde")]
        #[schemars(with = "String")]
        regex: Regex,
        form: Form,
    },
    /// `event.time` compared against `value` using `op`.  Negation is
    /// expressed by flipping the operator (e.g. `!(time >= X)` is
    /// `time < X`), so there is no separate [`Form`].
    TimeBound { op: TimeOp, value: DateTime<Utc> },
}

/// A predicate evaluated against a [`SourceId`] at source-selection
/// time, not against any individual event.
///
/// Today the only variant is regex-on-source-id, so the enum is
/// trivially extensible.  Kept as an enum (rather than a plain
/// struct) so future per-source predicates — matching on cached
/// metadata, file mtime windows, etc. — slot in alongside without
/// reshaping callers.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum SourcePredicate {
    /// The source's id matches (or, when `form` is `Negated`, does not
    /// match) the regex.  The engine skips entire sources whose ids
    /// fail rather than iterating their contents.
    SourceIdMatches {
        #[serde(with = "regex_serde")]
        #[schemars(with = "String")]
        regex: Regex,
        form: Form,
    },
}

/// Comparison operator for a [`EventPredicate::TimeBound`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema,
)]
pub enum TimeOp {
    /// `event.time >= value`
    AtLeast,
    /// `event.time > value`
    After,
    /// `event.time <= value`
    AtMost,
    /// `event.time < value`
    Before,
}

impl TimeOp {
    /// Returns the DSL token for this operator (e.g. `>=`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AtLeast => ">=",
            Self::After => ">",
            Self::AtMost => "<=",
            Self::Before => "<",
        }
    }
}

impl EventPredicate {
    pub fn matches_event(&self, event: &Event) -> bool {
        match self {
            Self::LevelAtLeast(threshold) => event.level >= *threshold,
            Self::LevelEquals { level, form } => {
                form.applied_to(event.level == *level)
            }
            Self::FieldEquals { name, value, form } => {
                form.applied_to(field_matches(event, name, value))
            }
            Self::MsgMatches { regex, form } => {
                form.applied_to(regex.is_match(&event.msg))
            }
            Self::TimeBound { op, value } => match op {
                TimeOp::AtLeast => event.time >= *value,
                TimeOp::After => event.time > *value,
                TimeOp::AtMost => event.time <= *value,
                TimeOp::Before => event.time < *value,
            },
        }
    }
}

impl SourcePredicate {
    pub fn matches_source_id(&self, source_id: &SourceId) -> bool {
        match self {
            Self::SourceIdMatches { regex, form } => {
                form.applied_to(regex.is_match(source_id.as_ref()))
            }
        }
    }
}

fn field_matches(event: &Event, name: &FieldName, value: &str) -> bool {
    match name {
        FieldName::Core(CoreField::Name) => {
            <crate::event::LoggerName as AsRef<str>>::as_ref(&event.name)
                == value
        }
        FieldName::Core(CoreField::Hostname) => {
            <crate::event::Hostname as AsRef<str>>::as_ref(&event.hostname)
                == value
        }
        FieldName::Core(CoreField::Pid) => event.pid.to_string() == value,
        FieldName::Core(CoreField::Msg) => event.msg == value,
        // Anything else is in `extra`.  Strings compare directly;
        // bools and numbers compare against the obvious lexical form
        // the user would type after seeing the JSON
        // (`true`/`false`, `1`, `1.5`); null gets a literal `"null"`
        // shorthand.  Arrays and objects don't have a useful lexical
        // equality and never match.
        FieldName::Extra(key) => match event.extra.get(key) {
            Some(serde_json::Value::Null) => value == "null",
            Some(serde_json::Value::String(s)) => s == value,
            Some(serde_json::Value::Bool(b)) => {
                value == if *b { "true" } else { "false" }
            }
            Some(serde_json::Value::Number(n)) => n.to_string() == value,
            Some(serde_json::Value::Array(_))
            | Some(serde_json::Value::Object(_))
            | None => false,
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
    #[error("invalid timestamp {value:?}: {error}")]
    BadTime { value: String, error: String },
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
    // must be probed first.  `!=` and `!~` come before `=`; `=~`, `==`,
    // `>=`, and `<=` likewise; bare `>` and `<` come after their `=`-
    // suffixed forms.
    if let Some((lhs, rhs)) = tok.split_once("=~") {
        return parse_regex_predicate(tok, lhs, rhs, Form::Affirmed);
    }
    if let Some((lhs, rhs)) = tok.split_once("!~") {
        return parse_regex_predicate(tok, lhs, rhs, Form::Negated);
    }
    if let Some((lhs, rhs)) = tok.split_once(">=") {
        require_nonempty_name(tok, lhs)?;
        return if lhs == "level" {
            Ok(EventPredicate::LevelAtLeast(parse_level(rhs)?).into())
        } else {
            parse_time_compare(lhs, rhs, TimeOp::AtLeast)
        };
    }
    if let Some((lhs, rhs)) = tok.split_once("<=") {
        require_nonempty_name(tok, lhs)?;
        return parse_time_compare(lhs, rhs, TimeOp::AtMost);
    }
    if let Some((lhs, rhs)) = tok.split_once("==") {
        return field_or_level(tok, lhs, rhs, Form::Affirmed);
    }
    if let Some((lhs, rhs)) = tok.split_once("!=") {
        return field_or_level(tok, lhs, rhs, Form::Negated);
    }
    if let Some((lhs, rhs)) = tok.split_once('>') {
        require_nonempty_name(tok, lhs)?;
        return parse_time_compare(lhs, rhs, TimeOp::After);
    }
    if let Some((lhs, rhs)) = tok.split_once('<') {
        require_nonempty_name(tok, lhs)?;
        return parse_time_compare(lhs, rhs, TimeOp::Before);
    }
    if let Some((lhs, rhs)) = tok.split_once('=') {
        return field_or_level(tok, lhs, rhs, Form::Affirmed);
    }
    Err(FilterParseError::NoOperator { token: tok.to_string() })
}

fn parse_time_compare(
    lhs: &str,
    rhs: &str,
    op: TimeOp,
) -> Result<Predicate, FilterParseError> {
    if lhs == "time" {
        Ok(EventPredicate::TimeBound { op, value: parse_time(rhs)? }.into())
    } else {
        Err(FilterParseError::UnsupportedFieldOp {
            name: lhs.to_string(),
            op: op.as_str().to_string(),
        })
    }
}

fn parse_time(s: &str) -> Result<DateTime<Utc>, FilterParseError> {
    DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)).map_err(
        |e| FilterParseError::BadTime {
            value: s.to_string(),
            error: e.to_string(),
        },
    )
}

fn parse_regex_predicate(
    tok: &str,
    lhs: &str,
    rhs: &str,
    form: Form,
) -> Result<Predicate, FilterParseError> {
    require_nonempty_name(tok, lhs)?;
    let regex = Regex::new(rhs)
        .map_err(|e| FilterParseError::BadRegex(e.to_string()))?;
    match lhs {
        "msg" => Ok(EventPredicate::MsgMatches { regex, form }.into()),
        "source_id" => {
            Ok(SourcePredicate::SourceIdMatches { regex, form }.into())
        }
        _ => Err(FilterParseError::UnsupportedFieldOp {
            name: lhs.to_string(),
            op: match form {
                Form::Affirmed => "=~",
                Form::Negated => "!~",
            }
            .to_string(),
        }),
    }
}

fn field_or_level(
    tok: &str,
    lhs: &str,
    rhs: &str,
    form: Form,
) -> Result<Predicate, FilterParseError> {
    require_nonempty_name(tok, lhs)?;
    if lhs == "level" {
        Ok(EventPredicate::LevelEquals { level: parse_level(rhs)?, form }
            .into())
    } else if lhs == "source_id" {
        // Source ids only support regex; equality of canonical paths
        // is rarely what you want, and silently routing the token to
        // FieldEquals would search inside `event.extra` and never
        // match.  Fail loudly with a hint at the supported operator.
        Err(FilterParseError::UnsupportedFieldOp {
            name: lhs.to_string(),
            op: match form {
                Form::Affirmed => "=",
                Form::Negated => "!=",
            }
            .to_string(),
        })
    } else {
        Ok(EventPredicate::FieldEquals {
            name: FieldName::from(lhs),
            value: rhs.to_string(),
            form,
        }
        .into())
    }
}

fn require_nonempty_name(tok: &str, lhs: &str) -> Result<(), FilterParseError> {
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
            Self::Event(p) => p.fmt(f),
            Self::Source(p) => p.fmt(f),
        }
    }
}

impl fmt::Display for EventPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LevelAtLeast(l) => {
                write!(f, "level>={}", level_token(*l))
            }
            Self::LevelEquals { level, form } => {
                let op = match form {
                    Form::Affirmed => "=",
                    Form::Negated => "!=",
                };
                write!(f, "level{}{}", op, level_token(*level))
            }
            Self::FieldEquals { name, value, form } => {
                let op = match form {
                    Form::Affirmed => "=",
                    Form::Negated => "!=",
                };
                write!(f, "{}{}{}", name, op, quote(value))
            }
            Self::MsgMatches { regex, form } => {
                let op = match form {
                    Form::Affirmed => "=~",
                    Form::Negated => "!~",
                };
                write!(f, "msg{}{}", op, quote(regex.as_str()))
            }
            Self::TimeBound { op, value } => {
                // RFC 3339 with `Z` rather than `+00:00` for round-trip
                // symmetry: shorter to read and parses identically.
                write!(
                    f,
                    "time{}{}",
                    op.as_str(),
                    value.to_rfc3339_opts(SecondsFormat::AutoSi, true),
                )
            }
        }
    }
}

impl fmt::Display for SourcePredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceIdMatches { regex, form } => {
                let op = match form {
                    Form::Affirmed => "=~",
                    Form::Negated => "!~",
                };
                write!(f, "source_id{}{}", op, quote(regex.as_str()))
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
        || s.chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '\\');
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
        assert!(EventPredicate::LevelAtLeast(Level::Info).matches_event(&e));
        assert!(EventPredicate::LevelAtLeast(Level::Debug).matches_event(&e));
        assert!(!EventPredicate::LevelAtLeast(Level::Warn).matches_event(&e));
    }

    #[test]
    fn level_equals_only_exact() {
        let e = base_event(); // info
        assert!(
            EventPredicate::LevelEquals {
                level: Level::Info,
                form: Form::Affirmed
            }
            .matches_event(&e)
        );
        assert!(
            !EventPredicate::LevelEquals {
                level: Level::Warn,
                form: Form::Affirmed
            }
            .matches_event(&e)
        );
    }

    #[test]
    fn field_equals_core_fields() {
        let e = base_event();
        assert!(
            EventPredicate::FieldEquals {
                name: "name".into(),
                value: "Nexus".into(),
                form: Form::Affirmed,
            }
            .matches_event(&e)
        );
        assert!(
            !EventPredicate::FieldEquals {
                name: "name".into(),
                value: "SledAgent".into(),
                form: Form::Affirmed,
            }
            .matches_event(&e)
        );
        assert!(
            EventPredicate::FieldEquals {
                name: "hostname".into(),
                value: "sled-01".into(),
                form: Form::Affirmed,
            }
            .matches_event(&e)
        );
        assert!(
            EventPredicate::FieldEquals {
                name: "pid".into(),
                value: "1234".into(),
                form: Form::Affirmed,
            }
            .matches_event(&e)
        );
    }

    #[test]
    fn field_equals_extra_field() {
        let e = base_event();
        assert!(
            EventPredicate::FieldEquals {
                name: "component".into(),
                value: "nexus".into(),
                form: Form::Affirmed,
            }
            .matches_event(&e)
        );
        assert!(
            !EventPredicate::FieldEquals {
                name: "component".into(),
                value: "sled-agent".into(),
                form: Form::Affirmed,
            }
            .matches_event(&e)
        );
    }

    /// Reproduces a real-world bug where adding `iteration=1` to a filter
    /// rejected log entries that obviously contained `"iteration":1`.
    /// The value in `extra` is `serde_json::Value::Number`, and
    /// `Value::PartialEq<&str>` only matches `Value::String`, so numeric
    /// (and boolean) extras must be compared against their lexical form.
    #[test]
    fn field_equals_numeric_and_boolean_extras() {
        let e = ev(r#"{
            "v": 0,
            "level": 30,
            "name": "Nexus",
            "hostname": "sled-01",
            "pid": 1234,
            "time": "2025-04-01T00:00:00Z",
            "msg": "activating",
            "iteration": 1,
            "ratio": 1.5,
            "enabled": true,
            "disabled": false
        }"#);

        let m = |name: &str, value: &str| {
            EventPredicate::FieldEquals {
                name: name.into(),
                value: value.into(),
                form: Form::Affirmed,
            }
            .matches_event(&e)
        };

        // Integer extras compare against the obvious decimal form.
        assert!(m("iteration", "1"));
        assert!(!m("iteration", "2"));
        assert!(!m("iteration", "1.0"));

        // Floating-point extras use serde_json's canonical repr.
        assert!(m("ratio", "1.5"));
        assert!(!m("ratio", "1.50"));

        // Booleans compare against the JSON literal spelling.
        assert!(m("enabled", "true"));
        assert!(!m("enabled", "false"));
        assert!(m("disabled", "false"));
        assert!(!m("disabled", "true"));
    }

    /// Arrays and objects have no obvious lexical equality form, so they
    /// must never match a string-valued predicate — including the empty
    /// string, `[]`, or `{}`.  Pinned down here so a future change that
    /// adds e.g. JSON-stringified comparison has to update this test.
    #[test]
    fn field_equals_array_and_object_extras_never_match() {
        let e = ev(r#"{
            "v": 0,
            "level": 30,
            "name": "Nexus",
            "hostname": "sled-01",
            "pid": 1234,
            "time": "2025-04-01T00:00:00Z",
            "msg": "m",
            "tags": ["a", "b"],
            "context": { "k": "v" }
        }"#);

        for value in ["", "[]", "[\"a\",\"b\"]", "a"] {
            assert!(
                !EventPredicate::FieldEquals {
                    name: "tags".into(),
                    value: value.into(),
                    form: Form::Affirmed,
                }
                .matches_event(&e),
                "tags should not match {value:?}",
            );
        }
        for value in ["", "{}", "{\"k\":\"v\"}"] {
            assert!(
                !EventPredicate::FieldEquals {
                    name: "context".into(),
                    value: value.into(),
                    form: Form::Affirmed,
                }
                .matches_event(&e),
                "context should not match {value:?}",
            );
        }
    }

    #[test]
    fn field_equals_missing_field_does_not_match() {
        let e = base_event();
        assert!(
            !EventPredicate::FieldEquals {
                name: "nope".into(),
                value: "anything".into(),
                form: Form::Affirmed,
            }
            .matches_event(&e)
        );
    }

    #[test]
    fn msg_matches_regex() {
        let e = base_event();
        assert!(
            EventPredicate::MsgMatches {
                regex: Regex::new("blueprint").unwrap(),
                form: Form::Affirmed,
            }
            .matches_event(&e)
        );
        assert!(
            EventPredicate::MsgMatches {
                regex: Regex::new("^blueprint exec").unwrap(),
                form: Form::Affirmed,
            }
            .matches_event(&e)
        );
        assert!(
            !EventPredicate::MsgMatches {
                regex: Regex::new("nope").unwrap(),
                form: Form::Affirmed,
            }
            .matches_event(&e)
        );
    }

    #[test]
    fn empty_filter_matches_everything() {
        let e = base_event();
        assert!(Filter::default().matches_event(&e));
    }

    #[test]
    fn filter_is_conjunction() {
        let e = base_event();
        let f = Filter {
            predicates: vec![
                EventPredicate::LevelAtLeast(Level::Info).into(),
                EventPredicate::FieldEquals {
                    name: "name".into(),
                    value: "Nexus".into(),
                    form: Form::Affirmed,
                }
                .into(),
            ],
        };
        assert!(f.matches_event(&e));

        let f = Filter {
            predicates: vec![
                EventPredicate::LevelAtLeast(Level::Info).into(),
                EventPredicate::FieldEquals {
                    name: "name".into(),
                    value: "Other".into(),
                    form: Form::Affirmed,
                }
                .into(),
            ],
        };
        assert!(!f.matches_event(&e));
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
            Predicate::Event(EventPredicate::LevelAtLeast(Level::Warn)),
        ));
    }

    #[test]
    fn parse_level_equals_single_and_double_eq() {
        for src in ["level=error", "level==error"] {
            let f = parse(src);
            assert!(matches!(
                f.predicates()[0],
                Predicate::Event(EventPredicate::LevelEquals {
                    level: Level::Error,
                    form: Form::Affirmed,
                }),
            ));
        }
    }

    #[test]
    fn parse_level_name_is_case_insensitive() {
        for src in ["level>=WARN", "level>=Warn", "level>=warn"] {
            let f = parse(src);
            assert!(matches!(
                f.predicates()[0],
                Predicate::Event(EventPredicate::LevelAtLeast(Level::Warn)),
            ));
        }
    }

    #[test]
    fn parse_field_equals() {
        let f = parse("name=Nexus");
        match &f.predicates()[0] {
            Predicate::Event(EventPredicate::FieldEquals {
                name,
                value,
                form,
            }) => {
                assert_eq!(*name, FieldName::Core(CoreField::Name));
                assert_eq!(value, "Nexus");
                assert_eq!(*form, Form::Affirmed);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_msg_regex() {
        let f = parse("msg=~oh.*no");
        match &f.predicates()[0] {
            Predicate::Event(EventPredicate::MsgMatches { regex, form }) => {
                assert_eq!(regex.as_str(), "oh.*no");
                assert_eq!(*form, Form::Affirmed);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_quoted_value_preserves_spaces() {
        let f = parse(r#"msg=~"oh no""#);
        match &f.predicates()[0] {
            Predicate::Event(EventPredicate::MsgMatches { regex, form }) => {
                assert_eq!(regex.as_str(), "oh no");
                assert_eq!(*form, Form::Affirmed);
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
            !EventPredicate::LevelEquals {
                level: Level::Info,
                form: Form::Negated
            }
            .matches_event(&e)
        );
        assert!(
            EventPredicate::LevelEquals {
                level: Level::Warn,
                form: Form::Negated
            }
            .matches_event(&e)
        );
    }

    #[test]
    fn field_equals_negated_inverts() {
        let e = base_event();
        assert!(
            !EventPredicate::FieldEquals {
                name: "name".into(),
                value: "Nexus".into(),
                form: Form::Negated,
            }
            .matches_event(&e)
        );
        assert!(
            EventPredicate::FieldEquals {
                name: "name".into(),
                value: "Other".into(),
                form: Form::Negated,
            }
            .matches_event(&e)
        );
    }

    #[test]
    fn field_equals_negated_missing_field_matches() {
        // `field_matches` is false when the field is missing; negated,
        // that becomes true.  This is the natural reading of "every
        // event without an `nope` field" but worth pinning down with a
        // test since it's a behavior that's easy to flip accidentally.
        let e = base_event();
        assert!(
            EventPredicate::FieldEquals {
                name: "nope".into(),
                value: "anything".into(),
                form: Form::Negated,
            }
            .matches_event(&e)
        );
    }

    #[test]
    fn msg_matches_negated_inverts() {
        let e = base_event();
        assert!(
            !EventPredicate::MsgMatches {
                regex: Regex::new("blueprint").unwrap(),
                form: Form::Negated,
            }
            .matches_event(&e)
        );
        assert!(
            EventPredicate::MsgMatches {
                regex: Regex::new("nope").unwrap(),
                form: Form::Negated,
            }
            .matches_event(&e)
        );
    }

    #[test]
    fn parse_field_not_equals() {
        let f = parse("name!=Nexus");
        match &f.predicates()[0] {
            Predicate::Event(EventPredicate::FieldEquals {
                name,
                value,
                form,
            }) => {
                assert_eq!(*name, FieldName::Core(CoreField::Name));
                assert_eq!(value, "Nexus");
                assert_eq!(*form, Form::Negated);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_level_not_equals() {
        let f = parse("level!=warn");
        match &f.predicates()[0] {
            Predicate::Event(EventPredicate::LevelEquals { level, form }) => {
                assert_eq!(*level, Level::Warn);
                assert_eq!(*form, Form::Negated);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_msg_not_matches() {
        let f = parse("msg!~oh.*no");
        match &f.predicates()[0] {
            Predicate::Event(EventPredicate::MsgMatches { regex, form }) => {
                assert_eq!(regex.as_str(), "oh.*no");
                assert_eq!(*form, Form::Negated);
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

    // ---------- parser errors ----------

    #[test]
    fn parse_unknown_level_errors() {
        let err: FilterParseError =
            "level>=loud".parse::<Filter>().unwrap_err();
        assert!(
            matches!(err, FilterParseError::BadLevel(ref s) if s == "loud")
        );
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
                EventPredicate::LevelAtLeast(Level::Warn).into(),
                EventPredicate::LevelEquals {
                    level: Level::Error,
                    form: Form::Affirmed,
                }
                .into(),
                EventPredicate::LevelEquals {
                    level: Level::Info,
                    form: Form::Negated,
                }
                .into(),
                EventPredicate::FieldEquals {
                    name: "name".into(),
                    value: "Nexus".into(),
                    form: Form::Affirmed,
                }
                .into(),
                EventPredicate::FieldEquals {
                    name: "hostname".into(),
                    value: "sled-01".into(),
                    form: Form::Negated,
                }
                .into(),
                EventPredicate::MsgMatches {
                    regex: Regex::new("foo.*").unwrap(),
                    form: Form::Affirmed,
                }
                .into(),
                EventPredicate::MsgMatches {
                    regex: Regex::new("bar.*").unwrap(),
                    form: Form::Negated,
                }
                .into(),
            ],
        };
        assert_eq!(
            f.to_string(),
            "level>=warn level=error level!=info name=Nexus \
             hostname!=sled-01 msg=~foo.* msg!~bar.*",
        );
    }

    #[test]
    fn display_quotes_values_with_spaces() {
        let f = Filter {
            predicates: vec![
                EventPredicate::FieldEquals {
                    name: "msg".into(),
                    value: "oh no".into(),
                    form: Form::Affirmed,
                }
                .into(),
            ],
        };
        // Some shlex versions emit single quotes, some emit double; we
        // care that the round-trip works, not the exact byte sequence.
        let parsed: Filter = f.to_string().parse().unwrap();
        match &parsed.predicates()[0] {
            Predicate::Event(EventPredicate::FieldEquals { value, .. }) => {
                assert_eq!(value, "oh no");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn display_then_parse_round_trip() {
        // Cover each predicate variant, in both forms where applicable.
        let inputs = [
            "level>=warn",
            "level=error",
            "level!=warn",
            "name=Nexus",
            "name!=Nexus",
            "component=nexus",
            "msg=~foo.*bar",
            "msg!~foo.*bar",
            "source_id=~nexus",
            "source_id!~debug",
            "level>=info name=Nexus msg=~boom",
            "time>=2026-05-09T12:00:00Z",
            "time>2026-05-09T12:00:00Z",
            "time<=2026-05-09T12:00:00Z",
            "time<2026-05-09T12:00:00Z",
            "time>=2026-05-01T00:00:00Z time<=2026-05-09T23:59:59Z",
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

    // ---------- source_id ----------

    fn sid(s: &str) -> SourceId {
        SourceId::from(s.to_string())
    }

    #[test]
    fn parse_source_id_regex() {
        let f = parse("source_id=~nexus");
        match &f.predicates()[0] {
            Predicate::Source(SourcePredicate::SourceIdMatches {
                regex,
                form,
            }) => {
                assert_eq!(regex.as_str(), "nexus");
                assert_eq!(*form, Form::Affirmed);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_source_id_negated_regex() {
        let f = parse("source_id!~debug");
        match &f.predicates()[0] {
            Predicate::Source(SourcePredicate::SourceIdMatches {
                regex,
                form,
            }) => {
                assert_eq!(regex.as_str(), "debug");
                assert_eq!(*form, Form::Negated);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_source_id_equality_errors() {
        // Source ids only support regex matching; a bare `=` would
        // silently route to FieldEquals and never match (no key
        // `source_id` lives in `extra`).  The parser must reject it.
        for src in ["source_id=foo", "source_id!=foo"] {
            let err = src.parse::<Filter>().unwrap_err();
            assert!(matches!(
                err,
                FilterParseError::UnsupportedFieldOp { ref name, .. }
                    if name == "source_id"
            ));
        }
    }

    #[test]
    fn source_id_matches_against_canonical_path() {
        let p = SourcePredicate::SourceIdMatches {
            regex: Regex::new("nexus").unwrap(),
            form: Form::Affirmed,
        };
        assert!(p.matches_source_id(&sid("/var/log/nexus.log")));
        assert!(!p.matches_source_id(&sid("/var/log/sled-agent.log")));
    }

    #[test]
    fn source_id_negated_inverts() {
        let p = SourcePredicate::SourceIdMatches {
            regex: Regex::new("debug").unwrap(),
            form: Form::Negated,
        };
        assert!(p.matches_source_id(&sid("/var/log/nexus.log")));
        assert!(!p.matches_source_id(&sid("/var/log/debug.log")));
    }

    #[test]
    fn filter_matches_source_id_is_conjunction() {
        // Two source-id predicates AND together; the source must
        // satisfy both.  Other predicate kinds don't constrain source
        // selection.
        let f: Filter =
            "source_id=~log source_id!~debug name=Nexus".parse().unwrap();
        assert!(f.matches_source_id(&sid("/var/log/nexus.log")));
        // Has "log" but also "debug" — second predicate rejects.
        assert!(!f.matches_source_id(&sid("/var/log/debug.log")));
        // Doesn't have "log" — first predicate rejects.
        assert!(!f.matches_source_id(&sid("/var/elsewhere/x.txt")));
    }

    #[test]
    fn empty_filter_accepts_every_source() {
        assert!(Filter::default().matches_source_id(&sid("anything")));
    }

    #[test]
    fn other_predicates_dont_constrain_source_id() {
        let f: Filter = "level>=warn name=Nexus msg=~boom".parse().unwrap();
        assert!(f.matches_source_id(&sid("/anything.log")));
    }

    // ---------- time bounds ----------

    fn ev_at(time: &str) -> Event {
        ev(&format!(
            r#"{{
                "v": 0,
                "level": 30,
                "name": "n",
                "hostname": "h",
                "pid": 1,
                "time": "{time}",
                "msg": "m"
            }}"#
        ))
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn time_bound_at_least_matches_at_and_after() {
        let cutoff = t("2026-05-09T12:00:00Z");
        let p =
            EventPredicate::TimeBound { op: TimeOp::AtLeast, value: cutoff };
        assert!(p.matches_event(&ev_at("2026-05-09T12:00:00Z")));
        assert!(p.matches_event(&ev_at("2026-05-09T12:00:01Z")));
        assert!(!p.matches_event(&ev_at("2026-05-09T11:59:59Z")));
    }

    #[test]
    fn time_bound_after_matches_strictly_after() {
        let cutoff = t("2026-05-09T12:00:00Z");
        let p = EventPredicate::TimeBound { op: TimeOp::After, value: cutoff };
        assert!(!p.matches_event(&ev_at("2026-05-09T12:00:00Z")));
        assert!(p.matches_event(&ev_at("2026-05-09T12:00:01Z")));
        assert!(!p.matches_event(&ev_at("2026-05-09T11:59:59Z")));
    }

    #[test]
    fn time_bound_at_most_matches_at_and_before() {
        let cutoff = t("2026-05-09T12:00:00Z");
        let p = EventPredicate::TimeBound { op: TimeOp::AtMost, value: cutoff };
        assert!(p.matches_event(&ev_at("2026-05-09T12:00:00Z")));
        assert!(!p.matches_event(&ev_at("2026-05-09T12:00:01Z")));
        assert!(p.matches_event(&ev_at("2026-05-09T11:59:59Z")));
    }

    #[test]
    fn time_bound_before_matches_strictly_before() {
        let cutoff = t("2026-05-09T12:00:00Z");
        let p = EventPredicate::TimeBound { op: TimeOp::Before, value: cutoff };
        assert!(!p.matches_event(&ev_at("2026-05-09T12:00:00Z")));
        assert!(!p.matches_event(&ev_at("2026-05-09T12:00:01Z")));
        assert!(p.matches_event(&ev_at("2026-05-09T11:59:59Z")));
    }

    #[test]
    fn time_bound_with_offset_input_matches_utc_event() {
        // Non-UTC offsets in the filter must be normalized to UTC
        // before comparison so equivalent instants compare equal.
        let p: Filter = "time>=2026-05-09T07:00:00-05:00".parse().unwrap();
        assert!(p.matches_event(&ev_at("2026-05-09T12:00:00Z")));
        assert!(!p.matches_event(&ev_at("2026-05-09T11:59:59Z")));
    }

    #[test]
    fn parse_time_bound_each_op() {
        let cases = [
            ("time>=2026-05-09T12:00:00Z", TimeOp::AtLeast),
            ("time>2026-05-09T12:00:00Z", TimeOp::After),
            ("time<=2026-05-09T12:00:00Z", TimeOp::AtMost),
            ("time<2026-05-09T12:00:00Z", TimeOp::Before),
        ];
        for (src, expected_op) in cases {
            let f = parse(src);
            match &f.predicates()[0] {
                Predicate::Event(EventPredicate::TimeBound { op, value }) => {
                    assert_eq!(*op, expected_op, "op for {src:?}");
                    assert_eq!(*value, t("2026-05-09T12:00:00Z"));
                }
                other => panic!("unexpected for {src:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn parse_bad_time_errors() {
        // Plain dates aren't accepted (RFC 3339 requires a time and
        // offset).  Missing offsets and arbitrary garbage also fail.
        for src in
            ["time>=2026-05-09", "time>=2026-05-09T12:00:00", "time>=tomorrow"]
        {
            let err = src.parse::<Filter>().unwrap_err();
            assert!(
                matches!(err, FilterParseError::BadTime { .. }),
                "expected BadTime for {src:?}, got {err:?}",
            );
        }
    }

    #[test]
    fn parse_time_inequality_on_other_field_errors() {
        // Only `time` accepts <, >, <=.  Any other lhs must be rejected
        // (and `>=` on a non-level/non-time lhs likewise).
        for (src, op) in [
            ("name<foo", "<"),
            ("name>foo", ">"),
            ("name<=foo", "<="),
            ("component>=bar", ">="),
        ] {
            let err = src.parse::<Filter>().unwrap_err();
            match err {
                FilterParseError::UnsupportedFieldOp {
                    op: actual_op, ..
                } => {
                    assert_eq!(actual_op, op, "for {src:?}");
                }
                other => panic!("unexpected for {src:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn display_time_bound_uses_z_suffix() {
        let p = EventPredicate::TimeBound {
            op: TimeOp::AtLeast,
            value: t("2026-05-09T12:00:00Z"),
        };
        let f = Filter { predicates: vec![p.into()] };
        assert_eq!(f.to_string(), "time>=2026-05-09T12:00:00Z");
    }

    #[test]
    fn time_bound_round_trips_through_serde() {
        let f = Filter {
            predicates: vec![
                EventPredicate::TimeBound {
                    op: TimeOp::AtLeast,
                    value: t("2026-05-01T00:00:00Z"),
                }
                .into(),
                EventPredicate::TimeBound {
                    op: TimeOp::Before,
                    value: t("2026-05-09T23:59:59Z"),
                }
                .into(),
            ],
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: Filter = serde_json::from_str(&json).unwrap();
        assert_eq!(f.to_string(), back.to_string());
    }

    #[test]
    fn time_bound_does_not_constrain_source_id() {
        let f: Filter = "time>=2026-05-09T00:00:00Z".parse().unwrap();
        assert!(f.matches_source_id(&sid("/anything.log")));
    }

    // ---------- serde ----------

    #[test]
    fn filter_round_trips_through_serde() {
        let f = Filter {
            predicates: vec![
                EventPredicate::LevelAtLeast(Level::Warn).into(),
                EventPredicate::FieldEquals {
                    name: "name".into(),
                    value: "Nexus".into(),
                    form: Form::Affirmed,
                }
                .into(),
                EventPredicate::MsgMatches {
                    regex: Regex::new("boom").unwrap(),
                    form: Form::Affirmed,
                }
                .into(),
                SourcePredicate::SourceIdMatches {
                    regex: Regex::new("nexus").unwrap(),
                    form: Form::Affirmed,
                }
                .into(),
            ],
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: Filter = serde_json::from_str(&json).unwrap();
        assert_eq!(f.to_string(), back.to_string());
    }

    // ---------- property tests ----------

    use proptest::prelude::*;

    /// Strategy: any [`Level`].
    fn arb_level() -> impl Strategy<Value = Level> {
        prop_oneof![
            Just(Level::Trace),
            Just(Level::Debug),
            Just(Level::Info),
            Just(Level::Warn),
            Just(Level::Error),
            Just(Level::Fatal),
        ]
    }

    /// Strategy: any [`Form`].
    fn arb_form() -> impl Strategy<Value = Form> {
        prop_oneof![Just(Form::Affirmed), Just(Form::Negated)]
    }

    /// Strategy: a [`FieldName`] over a small pool of core fields and
    /// extras.  The values overlap with what [`arb_event`] populates,
    /// so a non-trivial fraction of generated predicates actually
    /// match the generated events (the property test would be
    /// vacuously true if every predicate trivially failed).
    fn arb_field_name() -> impl Strategy<Value = FieldName> {
        prop_oneof![
            Just(FieldName::Core(CoreField::Name)),
            Just(FieldName::Core(CoreField::Hostname)),
            Just(FieldName::Core(CoreField::Pid)),
            Just(FieldName::Core(CoreField::Msg)),
            prop::sample::select(vec!["build", "component", "absent"])
                .prop_map(|s| FieldName::Extra(s.to_string())),
        ]
    }

    /// Strategy: a [`Regex`] from a small pool of patterns.  Building
    /// arbitrary valid regex syntax is more work than this property
    /// test warrants; a handful of fixed patterns gives the matcher a
    /// realistic mix of "matches" and "doesn't match" outcomes against
    /// the message corpus in [`arb_event`].
    fn arb_regex() -> impl Strategy<Value = Regex> {
        prop::sample::select(vec![
            "boom",
            "^starting",
            "blueprint",
            r"\d+",
            "[xyz]+",
            ".*",
        ])
        .prop_map(|src| Regex::new(src).unwrap())
    }

    /// Strategy: a [`TimeOp`].
    fn arb_time_op() -> impl Strategy<Value = TimeOp> {
        prop_oneof![
            Just(TimeOp::AtLeast),
            Just(TimeOp::After),
            Just(TimeOp::AtMost),
            Just(TimeOp::Before),
        ]
    }

    /// Strategy: a [`DateTime`] from a small pool overlapping with
    /// [`arb_event`]'s timestamps.
    fn arb_time() -> impl Strategy<Value = DateTime<Utc>> {
        prop::sample::select(vec![
            "2026-04-01T00:00:00Z",
            "2026-05-01T00:00:00Z",
            "2026-05-09T12:00:00Z",
            "2026-05-15T00:00:00Z",
        ])
        .prop_map(|s| {
            DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
        })
    }

    /// Strategy: an [`EventPredicate`] over all five variants.
    fn arb_event_predicate() -> impl Strategy<Value = EventPredicate> {
        prop_oneof![
            arb_level().prop_map(EventPredicate::LevelAtLeast),
            (arb_level(), arb_form()).prop_map(|(level, form)| {
                EventPredicate::LevelEquals { level, form }
            }),
            (
                arb_field_name(),
                arb_form(),
                prop::sample::select(vec![
                    "Nexus",
                    "SledAgent",
                    "h-1",
                    "absent-value",
                    "true",
                    "1234",
                    "0",
                ])
            )
                .prop_map(|(name, form, value)| {
                    EventPredicate::FieldEquals {
                        name,
                        value: value.to_string(),
                        form,
                    }
                }),
            (arb_regex(), arb_form()).prop_map(|(regex, form)| {
                EventPredicate::MsgMatches { regex, form }
            }),
            (arb_time_op(), arb_time()).prop_map(|(op, value)| {
                EventPredicate::TimeBound { op, value }
            }),
        ]
    }

    /// Strategy: a [`Predicate`] (event or source wrapper).
    fn arb_predicate() -> impl Strategy<Value = Predicate> {
        prop_oneof![
            arb_event_predicate().prop_map(Predicate::Event),
            (arb_regex(), arb_form()).prop_map(|(regex, form)| {
                Predicate::Source(SourcePredicate::SourceIdMatches {
                    regex,
                    form,
                })
            }),
        ]
    }

    /// Strategy: a [`Filter`] with 0-6 predicates.  Upper bound is
    /// small enough that proptest's shrinker produces readable
    /// counterexamples; the conjunction property doesn't need
    /// thousand-predicate filters to exercise.
    fn arb_filter() -> impl Strategy<Value = Filter> {
        prop::collection::vec(arb_predicate(), 0..=6)
            .prop_map(|predicates| Filter { predicates })
    }

    /// Strategy: an [`Event`] whose fields overlap with the values
    /// the predicate strategies generate.  Builds via JSON to reuse
    /// the existing serde plumbing.
    fn arb_event() -> impl Strategy<Value = Event> {
        (
            arb_level(),
            prop::sample::select(vec!["Nexus", "SledAgent"]),
            prop::sample::select(vec!["h-1", "h-2"]),
            prop::sample::select(vec![0u32, 1234, 9999]),
            prop::sample::select(vec![
                "starting up",
                "blueprint executed",
                "oh boom no",
                "xyz 42",
                "boring message",
            ]),
            arb_time(),
        )
            .prop_map(|(level, name, hostname, pid, msg, time)| {
                let level_num = level.as_bunyan_number();
                let json = format!(
                    r#"{{
                        "v": 0,
                        "level": {level_num},
                        "name": "{name}",
                        "hostname": "{hostname}",
                        "pid": {pid},
                        "time": "{}",
                        "msg": "{msg}",
                        "build": "0.1.0",
                        "component": "{name}"
                    }}"#,
                    time.to_rfc3339(),
                );
                serde_json::from_str(&json).expect("valid event JSON")
            })
    }

    proptest! {
        /// `Filter::matches` should reduce to the conjunction over
        /// the event-predicate subset: a filter accepts an event iff
        /// every [`EventPredicate`] inside it accepts the event.
        /// [`SourcePredicate`]s don't constrain events (they're
        /// evaluated separately at source-selection time) and so
        /// contribute `true` to the per-event conjunction.
        ///
        /// Regression cover for refactors that turn the `.all(...)`
        /// into `.any(...)`, drop one predicate kind, or short-circuit
        /// incorrectly.
        #[test]
        fn filter_matches_is_conjunction_over_event_predicates(
            filter in arb_filter(),
            event in arb_event(),
        ) {
            let direct = filter.matches_event(&event);
            let manual = filter
                .predicates()
                .iter()
                .filter_map(|p| match p {
                    Predicate::Event(ep) => Some(ep),
                    Predicate::Source(_) => None,
                })
                .all(|ep| ep.matches_event(&event));
            prop_assert_eq!(direct, manual);
        }
    }
}
