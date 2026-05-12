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
//!   field-visibility settings as the persisted session.
//!
//! The two modes are mutually exclusive at the CLI level.  Phase 1
//! adds the session-mode flags but leaves their execution stubbed out
//! with an "unimplemented" error — wiring follows in later phases.

use camino::Utf8PathBuf;
use clap::{ArgGroup, Parser, ValueEnum};
use seer::{
    Engine, Filter, HostnameDisplay, RenderOpts, SessionId, format_event,
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
    /// filter; in file mode it is the only filter.
    #[arg(short, long, default_value = "")]
    filter: String,

    /// Filter ANDed onto the resolved filter (session mode only).
    /// Mutually exclusive with `--filter`.
    #[arg(long, conflicts_with = "filter")]
    and_filter: Option<String>,

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    args.validate()?;

    // Session-mode execution is wired in a later phase.  Erroring
    // out here keeps the CLI surface available so docs and tooling
    // can refer to the flags, while making it obvious that
    // running in session mode today is a no-op.
    if args.session.is_some() {
        return Err("session mode is not yet implemented".into());
    }

    let filter: Filter = args.filter.parse()?;
    let opts = args.apply_overrides(Args::file_mode_defaults());

    let mut engine = Engine::new();
    for path in &args.files {
        engine.add_file_source(path)?;
    }

    let mut emitted: usize = 0;
    for result in engine.query_events(&filter) {
        match result {
            Ok(ee) => {
                for line in format_event(&ee.event, &opts) {
                    println!("{line}");
                }
                emitted += 1;
                if let Some(max) = args.count
                    && emitted >= max
                {
                    break;
                }
            }
            // SourceError's Display already says "I/O error: ...",
            // "failed to parse ...", or "warning: ..." as appropriate;
            // don't add another prefix here.
            Err(err) => eprintln!("{err}"),
        }
    }
    Ok(())
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
        assert_eq!(a.filter, "");
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
        assert_eq!(a.filter, "level>=warn");
        assert_eq!(a.count, Some(10));
    }

    #[test]
    fn parses_session_mode_with_stream() {
        let a =
            parse(&["seeit", "--session", "deadbeef", "--stream", "Nexus"])
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
            "seeit", "--session", "deadbeef", "--stream", "x", "--tab", "y",
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
        let a =
            parse(&["seeit", "foo.log", "--and-filter", "level>=warn"])
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
        // Wrong length and non-hex both fail parsing of SessionId.
        parse(&["seeit", "--session", "abc"]).unwrap_err();
        parse(&["seeit", "--session", "ghijklmn"]).unwrap_err();
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
