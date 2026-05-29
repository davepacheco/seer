// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `seeit`: a non-interactive companion to `seer`.
//!
//! Two modes:
//!
//! - **File mode** (today's behavior): given one or more bunyan log
//!   files on the command line, stream every matching event to stdout
//!   in a maximalist human-readable form.  Useful for development of
//!   the parser/filter/renderer and for grep-like scripting.
//! - **Session mode** (new): given a saved session id plus an optional
//!   selector (`--stream`, `--tab`, `--bookmark`), reproduce the lines
//!   `seer` would show for that target — same source set, filter, and
//!   field-visibility settings as the persisted session.  Useful for
//!   bug reports ("here's the exact `seeit` command that reproduces
//!   what I was looking at") and for piping a `seer` view to a file.
//!
//! The two modes are mutually exclusive at the CLI level.  `--header`
//! adds a one-line context banner on stderr so a human reader can
//! tell what was reproduced without polluting stdout.

use camino::Utf8PathBuf;
use clap::{ArgGroup, Parser, ValueEnum};
use seer::{
    Cursor, Engine, Filter, HostnameDisplay, MergeRecord, RenderOpts,
    ResolvedMode, ResolvedTarget, Selector, SessionId, SessionStore, Stepper,
    format_event, format_summary, resolve, summarize,
};

/// Clap-friendly mirror of [`HostnameDisplay`].
///
/// `HostnameDisplay` lives in the library and predates the CLI's need
/// for a `ValueEnum`; rather than push `clap` into the library's
/// dependency surface, the binary keeps its own enum and converts.
#[derive(Clone, Copy, Debug, ValueEnum)]
#[clap(rename_all = "lower")]
enum HostnameArg {
    Full,
    Short,
    None,
}

impl From<HostnameArg> for HostnameDisplay {
    fn from(a: HostnameArg) -> Self {
        match a {
            HostnameArg::Full => HostnameDisplay::Full,
            HostnameArg::Short => HostnameDisplay::Short,
            HostnameArg::None => HostnameDisplay::None,
        }
    }
}

/// Errors that surface only after clap has finished its own
/// constraint checking — the cross-arg invariants `clap`'s `requires`
/// machinery does not catch in our group layout (see [`Args::validate`]).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum ArgValidateError {
    /// One of `--stream` / `--tab` / `--bookmark` / `--before` /
    /// `--and-filter` was supplied without `--session`.
    #[error("`{flag}` requires `--session`")]
    SessionRequired {
        /// The flag the user supplied that needs session mode.
        flag: &'static str,
    },
}

#[derive(Parser, Debug)]
#[command(about = "non-interactive log explorer; companion to `seer`")]
#[command(group = ArgGroup::new("input")
    .required(true)
    .args(["files", "session"]))]
struct Args {
    /// One or more bunyan log files to read, in order.  Mutually
    /// exclusive with `--session`.
    #[arg(conflicts_with = "session")]
    files: Vec<Utf8PathBuf>,

    /// Session id (8-char hex) to reproduce a saved view from.
    /// Mutually exclusive with positional files.
    #[arg(long)]
    session: Option<SessionId>,

    /// Reproduce the named log stream from the start of its filtered
    /// view.  Requires `--session`.
    #[arg(long, conflicts_with_all = ["tab", "bookmark"])]
    stream: Option<String>,

    /// Reproduce the named tab at its saved cursor position.
    /// Requires `--session`.
    #[arg(long, conflicts_with_all = ["stream", "bookmark"])]
    tab: Option<String>,

    /// Reproduce starting at the named bookmark (name or UUID
    /// prefix).  Requires `--session`.
    #[arg(long, conflicts_with_all = ["stream", "tab"])]
    bookmark: Option<String>,

    /// Stop after emitting N records (default: unbounded).
    #[arg(long)]
    count: Option<usize>,

    /// Emit N records before the start position (e.g., for context
    /// around a bookmark).  Requires `--session`.
    #[arg(long)]
    before: Option<usize>,

    /// Filter expression, e.g. `level>=warn name=Nexus msg=~boom
    /// time>=2026-05-09T00:00:00Z`.  See `seer::filter` docs for the
    /// full grammar.  In session mode this replaces the resolved
    /// filter; in file mode it is the only filter.  Distinct from
    /// `--filter ""` (an explicit empty filter that clears the
    /// resolved one) which is unusual but expressible.
    #[arg(short, long)]
    filter: Option<String>,

    /// Filter ANDed onto the resolved filter (session mode only).
    /// Mutually exclusive with `--filter`.
    #[arg(long, conflicts_with = "filter")]
    and_filter: Option<String>,

    /// Print a one-line context banner to stderr describing what
    /// was reproduced (session id, selector, filter, mode).
    /// Stdout is untouched, so the same invocation still pipes
    /// cleanly to grep, diff, etc.
    #[arg(long)]
    header: bool,

    /// Render `key = value` extras below each event's header.
    #[arg(long, overrides_with = "no_extras")]
    show_extras: bool,
    /// Hide extras.
    #[arg(long, overrides_with = "show_extras")]
    no_extras: bool,

    /// Hostname rendering: `full`, `short`, or `none`.
    #[arg(long, value_enum)]
    hostname: Option<HostnameArg>,

    /// Show the `YYYY-MM-DD` date prefix on timestamps.
    #[arg(long, overrides_with = "no_date")]
    show_date: bool,
    /// Hide the date prefix.
    #[arg(long, overrides_with = "show_date")]
    no_date: bool,

    /// Show the bunyan `pid` field in the header.
    #[arg(long, overrides_with = "no_pid")]
    show_pid: bool,
    /// Hide the pid field.
    #[arg(long, overrides_with = "show_pid")]
    no_pid: bool,

    /// Show the bunyan `name` (logger) field in the header.
    #[arg(long, overrides_with = "no_name")]
    show_name: bool,
    /// Hide the name field.
    #[arg(long, overrides_with = "show_name")]
    no_name: bool,

    /// Render records as raw line bytes from the source.
    #[arg(long, overrides_with = "no_raw")]
    show_raw: bool,
    /// Render the formatted layout instead of raw bytes.
    #[arg(long, overrides_with = "show_raw")]
    no_raw: bool,
}

impl Args {
    /// Translates the at-most-one selector flag into a [`Selector`].
    ///
    /// `Args::validate` has already enforced that at most one of
    /// `stream`, `tab`, `bookmark` is set; with none of them set the
    /// session-mode default is [`Selector::WholeSession`].
    fn selector(&self) -> Selector {
        if let Some(name) = &self.stream {
            Selector::Stream(name.clone())
        } else if let Some(name) = &self.tab {
            Selector::Tab(name.clone())
        } else if let Some(needle) = &self.bookmark {
            Selector::Bookmark(needle.clone())
        } else {
            Selector::WholeSession
        }
    }

    /// Verifies cross-arg invariants that clap's `requires` machinery
    /// does not enforce in our group layout.
    ///
    /// Specifically: `--stream`, `--tab`, `--bookmark`, `--before`,
    /// and `--and-filter` all imply session mode, so each is rejected
    /// when `--session` is absent.  clap 4 enforces a group's
    /// `required` membership but treats `requires` on individual args
    /// as advisory when another member of the input group is
    /// present — so the check has to live here, not in the derive.
    fn validate(&self) -> Result<(), ArgValidateError> {
        if self.session.is_none() {
            if self.stream.is_some() {
                return Err(ArgValidateError::SessionRequired {
                    flag: "--stream",
                });
            }
            if self.tab.is_some() {
                return Err(ArgValidateError::SessionRequired {
                    flag: "--tab",
                });
            }
            if self.bookmark.is_some() {
                return Err(ArgValidateError::SessionRequired {
                    flag: "--bookmark",
                });
            }
            if self.before.is_some() {
                return Err(ArgValidateError::SessionRequired {
                    flag: "--before",
                });
            }
            if self.and_filter.is_some() {
                return Err(ArgValidateError::SessionRequired {
                    flag: "--and-filter",
                });
            }
        }
        Ok(())
    }

    /// Render-opts seed used in file mode.
    ///
    /// File mode predates session mode and has always shown every
    /// available column; preserving that means new invocations
    /// without any `--show-*` / `--no-*` flags get the same output
    /// they used to.
    fn file_mode_defaults() -> RenderOpts {
        RenderOpts {
            show_extras: true,
            show_date: true,
            hostname: HostnameDisplay::Full,
            show_pid: true,
            show_name: true,
            show_raw: false,
        }
    }

    /// Applies each `--show-X` / `--no-X` flag to `base`.
    ///
    /// `overrides_with` on each pair ensures at most one of the two
    /// bools is set, so the `if/else if` ladder is unambiguous: a
    /// pair with neither flag leaves the corresponding field of
    /// `base` untouched.
    fn apply_overrides(&self, mut base: RenderOpts) -> RenderOpts {
        if self.show_extras {
            base.show_extras = true;
        } else if self.no_extras {
            base.show_extras = false;
        }
        if self.show_date {
            base.show_date = true;
        } else if self.no_date {
            base.show_date = false;
        }
        if let Some(h) = self.hostname {
            base.hostname = h.into();
        }
        if self.show_pid {
            base.show_pid = true;
        } else if self.no_pid {
            base.show_pid = false;
        }
        if self.show_name {
            base.show_name = true;
        } else if self.no_name {
            base.show_name = false;
        }
        if self.show_raw {
            base.show_raw = true;
        } else if self.no_raw {
            base.show_raw = false;
        }
        base
    }
}

fn main() {
    let args = Args::parse();
    if let Err(err) = run(&args) {
        report_error(&*err);
        std::process::exit(1);
    }
}

/// Top-level entry point separated from `main` so error handling can
/// route through [`report_error`].  `main` returning
/// `Result<_, Box<dyn Error>>` would format failures via `Debug`,
/// which produces opaque enum variants instead of the carefully-
/// worded `Display` strings each error type defines.
fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    args.validate().map_err(boxed_err)?;
    match args.session {
        Some(session_id) => run_session_mode(args, session_id),
        None => run_file_mode(args),
    }
}

/// Boxes any [`Error`] into the `Box<dyn Error>` shape `run`
/// returns.  Used at error-conversion points where the type-erased
/// box wants an explicit conversion.
fn boxed_err<E: std::error::Error + Send + Sync + 'static>(
    e: E,
) -> Box<dyn std::error::Error> {
    Box::new(e)
}

/// Prints `err` and any chained causes to stderr as `Display`
/// strings.  Each cause appears on its own indented line, so a
/// resolve failure with a wrapped I/O cause looks like:
///
/// ```text
/// seeit: source /log/a has changed since the session was saved: …
///   caused by: No such file or directory (os error 2)
/// ```
fn report_error(err: &dyn std::error::Error) {
    eprintln!("seeit: {err}");
    let mut cause = err.source();
    while let Some(c) = cause {
        eprintln!("  caused by: {c}");
        cause = c.source();
    }
}

/// File-only mode: positional files plus optional filter and
/// overrides, no persisted session involved.  Preserves the
/// pre-session-mode behavior — maximalist defaults, no count cap —
/// when no new flags are passed.
fn run_file_mode(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let filter: Filter = args.filter.as_deref().unwrap_or("").parse()?;
    let opts = args.apply_overrides(Args::file_mode_defaults());

    let mut engine = Engine::new();
    for path in &args.files {
        engine.add_file_source(path)?;
    }

    emit_forward_from_engine(
        &engine,
        &filter,
        &Cursor::default(),
        args.count,
        &opts,
    );
    Ok(())
}

/// Session mode: load `session_id`, resolve the selector, apply CLI
/// overrides, and emit either records or a summary per the resolved
/// mode.
fn run_session_mode(
    args: &Args,
    session_id: SessionId,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = SessionStore::open()?;
    let selector = args.selector();
    let resolved = resolve(&store, session_id, &selector)?;

    let filter = combine_filter(args, &resolved.filter)?;
    let opts = args.apply_overrides(resolved.render_opts);

    let mut engine = Engine::new();
    for path in &resolved.sources {
        engine.add_file_source(path)?;
    }

    if args.header {
        print_header(session_id, args, &selector, &filter, &resolved);
    }

    match resolved.mode {
        ResolvedMode::Summary => emit_summary(&engine, &filter),
        ResolvedMode::Records => {
            emit_records_window(&engine, &filter, &resolved, args, &opts)
        }
    }
    Ok(())
}

/// Writes a one-line context banner to stderr.  Stdout is left
/// alone so the binary's output remains pipeable.
///
/// The banner names what was reproduced: session id, selector
/// (whole-session / stream / tab / bookmark), final filter, and
/// mode.  Source paths are summarized by count to keep the line
/// reasonable on sessions with dozens of files.
fn print_header(
    session_id: SessionId,
    args: &Args,
    selector: &Selector,
    filter: &Filter,
    resolved: &ResolvedTarget,
) {
    let selector_str = match selector {
        Selector::WholeSession => "whole-session".to_string(),
        Selector::Stream(name) => format!("stream={name:?}"),
        Selector::Tab(name) => format!("tab={name:?}"),
        Selector::Bookmark(needle) => format!("bookmark={needle:?}"),
    };
    let mode_str = match resolved.mode {
        ResolvedMode::Records => "records",
        ResolvedMode::Summary => "summary",
    };
    let filter_str = if filter.predicates().is_empty() {
        "<none>".to_string()
    } else {
        // The grammar is the same `seer::filter` parses, so render
        // the predicates back via Debug on the slice; a richer
        // round-trip Display would be a separate piece of work.
        format!("{:?}", filter.predicates())
    };
    let window = match (args.before, args.count) {
        (None, None) => "unbounded".to_string(),
        (Some(b), None) => format!("before={b} count=unbounded"),
        (None, Some(c)) => format!("count={c}"),
        (Some(b), Some(c)) => format!("before={b} count={c}"),
    };
    eprintln!(
        "seeit: session={} target={} mode={} sources={} filter={} window={}",
        session_id,
        selector_str,
        mode_str,
        resolved.sources.len(),
        filter_str,
        window,
    );
}

/// Walks forward from `cursor` and emits each event via
/// [`format_event`].  Stops after `count` records when set, or when
/// the engine is exhausted.  Errors are printed to stderr and don't
/// count toward `count`.
fn emit_forward_from_engine(
    engine: &Engine,
    filter: &Filter,
    cursor: &Cursor,
    count: Option<usize>,
    opts: &RenderOpts,
) {
    let mut stepper = engine.stepper(filter.clone(), cursor);
    let mut emitted: usize = 0;
    while let Some(r) = stepper.step_forward() {
        emit_record(&r, opts);
        if r.event().is_ok() {
            emitted += 1;
            if let Some(max) = count
                && emitted >= max
            {
                break;
            }
        }
    }
}

/// Records-mode session emission: `--before N` records strictly
/// before the resolved cursor (chronological order), then up to
/// `--count M` records starting at the cursor.
///
/// Uses two steppers because a single stepper owns one position that
/// both directions share — see [`step_backward_n`].
fn emit_records_window(
    engine: &Engine,
    filter: &Filter,
    resolved: &ResolvedTarget,
    args: &Args,
    opts: &RenderOpts,
) {
    if let Some(before) = args.before {
        let mut back = engine.stepper(filter.clone(), &resolved.cursor);
        for r in step_backward_n(&mut back, before) {
            emit_record(&r, opts);
        }
    }
    emit_forward_from_engine(
        engine,
        filter,
        &resolved.cursor,
        args.count,
        opts,
    );
}

/// Walks `stepper` backward up to `n` times and returns the records
/// in chronological (forward) order.
///
/// Stops early if [`Stepper::step_backward`] returns `None`, so the
/// returned vec is at most `n` long.  After this call the stepper's
/// cursor has retreated by the returned vec's length; callers that
/// want to resume forward from the *original* starting position
/// should construct a fresh stepper at the same cursor.  Used by
/// [`emit_records_window`] to assemble `--before N`'s pre-cursor
/// window.
fn step_backward_n(stepper: &mut Stepper, n: usize) -> Vec<MergeRecord> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        match stepper.step_backward() {
            Some(r) => out.push(r),
            None => break,
        }
    }
    out.reverse();
    out
}

/// Builds a [`Summary`] over `filter` and prints it.  Summary mode
/// ignores `--before` and `--count` — those are records-mode
/// concepts; the summary covers the whole filtered set.
fn emit_summary(engine: &Engine, filter: &Filter) {
    let summary = summarize(engine, filter);
    for line in format_summary(&summary) {
        println!("{line}");
    }
}

/// Renders a single [`MergeRecord`].  Successful events print their
/// formatted lines to stdout; per-line parse/IO errors print their
/// `Display` form to stderr (matches the legacy file-mode behavior).
fn emit_record(r: &MergeRecord, opts: &RenderOpts) {
    match r.event() {
        Ok(e) => {
            for line in format_event(e, opts) {
                println!("{line}");
            }
        }
        // The MergeError's Display already includes context; no
        // extra prefix here.
        Err(err) => eprintln!("{err}"),
    }
}

/// Combines the CLI's `--filter` / `--and-filter` overrides with the
/// resolved filter from a session selector.
///
/// Returns:
/// - The resolved filter clone when neither override is set.
/// - A parsed standalone filter when `--filter` is set.
/// - The resolved filter with the parsed `--and-filter` predicates
///   appended when `--and-filter` is set.
///
/// Validation has already ruled out the case where both overrides
/// are set simultaneously.
fn combine_filter(
    args: &Args,
    base: &Filter,
) -> Result<Filter, seer::FilterParseError> {
    if let Some(s) = &args.filter {
        return s.parse();
    }
    if let Some(s) = &args.and_filter {
        let mut combined = base.clone();
        let extra: Filter = s.parse()?;
        for p in extra.predicates() {
            combined.add_predicate(p.clone());
        }
        return Ok(combined);
    }
    Ok(base.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(argv)
    }

    #[test]
    fn clap_invariants() {
        Args::command().debug_assert();
    }

    #[test]
    fn parses_file_mode() {
        let a = parse(&["seeit", "foo.log"]).unwrap();
        assert_eq!(a.files, vec![Utf8PathBuf::from("foo.log")]);
        assert!(a.session.is_none());
        assert_eq!(a.filter, None);
        assert_eq!(a.count, None);
    }

    #[test]
    fn parses_file_mode_with_multiple_files_and_filter() {
        let a = parse(&[
            "seeit",
            "a.log",
            "b.log",
            "--filter",
            "level>=warn",
            "--count",
            "10",
        ])
        .unwrap();
        assert_eq!(
            a.files,
            vec![Utf8PathBuf::from("a.log"), Utf8PathBuf::from("b.log")]
        );
        assert_eq!(a.filter.as_deref(), Some("level>=warn"));
        assert_eq!(a.count, Some(10));
    }

    #[test]
    fn parses_session_mode_with_stream() {
        let a = parse(&["seeit", "--session", "deadbeef", "--stream", "Nexus"])
            .unwrap();
        assert_eq!(a.session.map(|s| s.to_string()), Some("deadbeef".into()));
        assert_eq!(a.stream.as_deref(), Some("Nexus"));
        assert!(a.tab.is_none());
        assert!(a.bookmark.is_none());
    }

    #[test]
    fn parses_session_mode_with_tab() {
        let a = parse(&["seeit", "--session", "deadbeef", "--tab", "Tab 1"])
            .unwrap();
        assert_eq!(a.tab.as_deref(), Some("Tab 1"));
    }

    #[test]
    fn parses_session_mode_with_bookmark_and_before() {
        let a = parse(&[
            "seeit",
            "--session",
            "deadbeef",
            "--bookmark",
            "panic",
            "--before",
            "5",
            "--count",
            "20",
        ])
        .unwrap();
        assert_eq!(a.bookmark.as_deref(), Some("panic"));
        assert_eq!(a.before, Some(5));
        assert_eq!(a.count, Some(20));
    }

    #[test]
    fn parses_session_mode_with_and_filter() {
        let a = parse(&[
            "seeit",
            "--session",
            "deadbeef",
            "--and-filter",
            "level>=error",
        ])
        .unwrap();
        assert_eq!(a.and_filter.as_deref(), Some("level>=error"));
    }

    #[test]
    fn rejects_empty_args() {
        // Neither files nor --session supplied.
        parse(&["seeit"]).unwrap_err();
    }

    #[test]
    fn rejects_files_and_session_together() {
        parse(&["seeit", "foo.log", "--session", "deadbeef"]).unwrap_err();
    }

    #[test]
    fn rejects_selector_without_session() {
        // clap's own `requires` machinery doesn't fire when the input
        // group is satisfied by `files`, so these go through clap
        // cleanly and are caught by `Args::validate`.
        for (flag, value) in
            [("--stream", "Nexus"), ("--tab", "Tab 1"), ("--bookmark", "panic")]
        {
            let a = parse(&["seeit", "foo.log", flag, value]).unwrap();
            assert_eq!(
                a.validate(),
                Err(ArgValidateError::SessionRequired { flag })
            );
        }
    }

    #[test]
    fn rejects_multiple_selectors() {
        parse(&[
            "seeit",
            "--session",
            "deadbeef",
            "--stream",
            "x",
            "--tab",
            "y",
        ])
        .unwrap_err();
        parse(&[
            "seeit",
            "--session",
            "deadbeef",
            "--stream",
            "x",
            "--bookmark",
            "z",
        ])
        .unwrap_err();
        parse(&[
            "seeit",
            "--session",
            "deadbeef",
            "--tab",
            "y",
            "--bookmark",
            "z",
        ])
        .unwrap_err();
    }

    #[test]
    fn rejects_and_filter_without_session() {
        let a = parse(&["seeit", "foo.log", "--and-filter", "level>=warn"])
            .unwrap();
        assert_eq!(
            a.validate(),
            Err(ArgValidateError::SessionRequired { flag: "--and-filter" })
        );
    }

    #[test]
    fn rejects_before_without_session() {
        let a = parse(&["seeit", "foo.log", "--before", "5"]).unwrap();
        assert_eq!(
            a.validate(),
            Err(ArgValidateError::SessionRequired { flag: "--before" })
        );
    }

    #[test]
    fn validate_accepts_file_mode_and_session_mode() {
        // No selectors plus no --session: valid (file mode).
        parse(&["seeit", "foo.log"]).unwrap().validate().unwrap();
        // Selectors plus --session: valid.
        parse(&["seeit", "--session", "deadbeef", "--stream", "x"])
            .unwrap()
            .validate()
            .unwrap();
        parse(&["seeit", "--session", "deadbeef", "--before", "5"])
            .unwrap()
            .validate()
            .unwrap();
    }

    #[test]
    fn rejects_filter_and_and_filter_together() {
        parse(&[
            "seeit",
            "--session",
            "deadbeef",
            "--filter",
            "x=y",
            "--and-filter",
            "level>=warn",
        ])
        .unwrap_err();
    }

    #[test]
    fn rejects_invalid_session_id() {
        // Empty and out-of-alphabet inputs both fail parsing of
        // SessionId.
        parse(&["seeit", "--session", ""]).unwrap_err();
        parse(&["seeit", "--session", "abcdef-h"]).unwrap_err();
    }

    #[test]
    fn file_mode_defaults_match_legacy_maximalist_render() {
        // Phase 1's contract: file mode with no override flags emits
        // exactly what today's `seeit` did — every column on, full
        // hostname, raw off.
        let opts = Args::file_mode_defaults();
        assert!(opts.show_extras);
        assert!(opts.show_date);
        assert_eq!(opts.hostname, HostnameDisplay::Full);
        assert!(opts.show_pid);
        assert!(opts.show_name);
        assert!(!opts.show_raw);
    }

    #[test]
    fn render_override_flags_flip_individual_fields() {
        let a = parse(&[
            "seeit",
            "foo.log",
            "--no-extras",
            "--no-pid",
            "--hostname",
            "short",
        ])
        .unwrap();
        let opts = a.apply_overrides(Args::file_mode_defaults());
        assert!(!opts.show_extras);
        assert!(!opts.show_pid);
        assert_eq!(opts.hostname, HostnameDisplay::Short);
        // Untouched fields keep the file-mode default.
        assert!(opts.show_date);
        assert!(opts.show_name);
    }

    #[test]
    fn render_override_hostname_none() {
        let a = parse(&["seeit", "foo.log", "--hostname", "none"]).unwrap();
        let opts = a.apply_overrides(Args::file_mode_defaults());
        assert_eq!(opts.hostname, HostnameDisplay::None);
    }

    #[test]
    fn render_override_show_raw() {
        let a = parse(&["seeit", "foo.log", "--show-raw"]).unwrap();
        let opts = a.apply_overrides(Args::file_mode_defaults());
        assert!(opts.show_raw);
    }

    #[test]
    fn show_and_no_pair_overrides_to_last() {
        // clap's `overrides_with` resolves a `--show-extras --no-extras`
        // pair to whichever came last on the command line.
        let a = parse(&["seeit", "foo.log", "--show-extras", "--no-extras"])
            .unwrap();
        assert!(!a.show_extras);
        assert!(a.no_extras);

        let a = parse(&["seeit", "foo.log", "--no-extras", "--show-extras"])
            .unwrap();
        assert!(a.show_extras);
        assert!(!a.no_extras);
    }
}
