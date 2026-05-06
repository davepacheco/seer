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
use seer::Engine;

#[derive(Parser)]
#[command(
    about = "non-interactive log explorer; companion to `seer`"
)]
struct Args {
    /// One or more bunyan log files to read, in order.
    #[arg(required = true)]
    files: Vec<Utf8PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut engine = Engine::new();
    for path in &args.files {
        engine.add_file_source(path)?;
    }
    for result in engine.query_events() {
        match result {
            Ok(event) => println!(
                "{} [{}] {}/{}/{}: {}",
                event.time.to_rfc3339(),
                event.level,
                event.name,
                event.hostname,
                event.pid,
                event.msg,
            ),
            Err(err) => eprintln!("error: {err}"),
        }
    }
    Ok(())
}
