// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `seeit`: a non-interactive companion to `seer`.
//!
//! Reads one or more bunyan log files via the engine and dumps every
//! event to stdout in a simple human-readable format.  Errors that occur
//! while reading individual lines are reported to stderr; processing
//! continues with the next line.

use camino::Utf8PathBuf;
use clap::Parser;
use seer::{Engine, Filter, HostnameDisplay, RenderOpts, format_event};

#[derive(Parser)]
#[command(about = "non-interactive log explorer; companion to `seer`")]
struct Args {
    /// One or more bunyan log files to read, in order.
    #[arg(required = true)]
    files: Vec<Utf8PathBuf>,

    /// Filter expression, e.g. `level>=warn name=Nexus msg=~boom
    /// time>=2026-05-09T00:00:00Z`.  See `seer::filter` docs for the
    /// full grammar.
    #[arg(short, long, default_value = "")]
    filter: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let filter: Filter = args.filter.parse()?;

    let mut engine = Engine::new();
    for path in &args.files {
        engine.add_file_source(path)?;
    }
    for result in engine.query_events(&filter) {
        match result {
            Ok(ee) => {
                // `seeit` is non-interactive and has no toggles, so
                // it prints the most informative form: every column
                // present, full hostname, date prefix on, extras
                // shown.
                let opts = RenderOpts {
                    show_extras: true,
                    show_date: true,
                    hostname: HostnameDisplay::Full,
                    show_pid: true,
                    show_name: true,
                };
                for line in format_event(&ee.event, &opts) {
                    println!("{line}");
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
