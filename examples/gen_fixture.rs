// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generates a sample bunyan log file used for manual testing of
//! `seer` and `seeit`.
//!
//! Run with:
//!
//! ```text
//! cargo run --example gen_fixture -- tests/fixtures/sample.log
//! ```
//!
//! Records are emitted through `slog` + `slog-bunyan` so the output
//! exactly matches the bunyan format the engine consumes.  `hostname`
//! and `pid` are taken from the running process; everything else is
//! hand-constructed to give a useful mix of levels, components, and
//! structured fields for filter exercises.

use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use slog::{Drain, Logger, debug, error, info, o, warn};
use std::fs::File;
use std::sync::Mutex;

#[derive(Parser)]
struct Args {
    /// Output path; truncated if it already exists.
    #[arg(default_value = "tests/fixtures/sample.log")]
    output: Utf8PathBuf,
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();
    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    File::create(&args.output)?; // truncate

    nexus_block(&args.output)?;
    sled_agent_block(&args.output, "sled-01")?;
    sled_agent_block(&args.output, "sled-02")?;
    sled_agent_block(&args.output, "sled-03")?;
    mgs_block(&args.output)?;
    crdb_block(&args.output)?;

    let count = std::fs::read_to_string(&args.output)?.lines().count();
    println!("wrote {count} records to {}", args.output);
    Ok(())
}

fn open_drain(path: &Utf8Path, name: &'static str) -> Logger {
    let file = File::options()
        .append(true)
        .open(path)
        .expect("open output file");
    let drain = slog_bunyan::with_name(name, file).build().fuse();
    let drain = Mutex::new(drain).fuse();
    Logger::root(drain, o!())
}

fn nexus_block(path: &Utf8Path) -> std::io::Result<()> {
    let log = open_drain(path, "Nexus");
    info!(log, "Nexus starting up";
          "build" => "0.1.0", "version" => "0.1.0");
    info!(log, "loaded blueprint"; "blueprint_id" => "1f8d-3c57");
    info!(log, "DNS records loaded"; "zones" => 4);
    info!(log, "instance_watcher background task started");
    info!(log, "blueprint_executor background task started");

    // Simulated request traffic, with occasional slow ones.
    for i in 0..50_u32 {
        let req_id = format!("req-{i:04}");
        let req_log = log.new(o!("request_id" => req_id));
        info!(req_log, "received request";
              "method" => "GET",
              "uri" => "/v1/instances");
        let elapsed = 20 + (i * 13) % 120;
        if elapsed > 100 {
            warn!(req_log, "slow request"; "elapsed_ms" => elapsed);
        }
        info!(req_log, "request complete";
              "status" => 200,
              "elapsed_ms" => elapsed);
    }

    error!(log, "blueprint execution failed";
           "blueprint_id" => "1f8d-3c57",
           "step" => "datasets",
           "reason" => "sled-04 unreachable");
    error!(log, "blueprint execution failed";
           "blueprint_id" => "1f8d-3c57",
           "step" => "datasets",
           "reason" => "sled-04 unreachable");
    info!(log, "blueprint execution retry scheduled";
          "blueprint_id" => "1f8d-3c57",
          "delay_s" => 30);
    info!(log, "blueprint execution succeeded";
          "blueprint_id" => "1f8d-3c57");

    debug!(log, "lookup expanded";
           "kind" => "instance", "id" => "abc-123");
    debug!(log, "authz check";
           "actor" => "user-foo",
           "action" => "read",
           "resource" => "instance:abc-123");
    Ok(())
}

fn sled_agent_block(
    path: &Utf8Path,
    sled: &'static str,
) -> std::io::Result<()> {
    let log = open_drain(path, "SledAgent").new(o!("sled" => sled));
    info!(log, "sled-agent starting up");
    info!(log, "rack initialized");

    for i in 0..20_u32 {
        let inst = format!("inst-{i:04}");
        let inst_log = log.new(o!("instance_id" => inst));
        debug!(inst_log, "received instance ensure";
               "kind" => "running");
        info!(inst_log, "instance starting";
              "image" => "alpine-3.19");
        info!(inst_log, "instance running";
              "vcpus" => 2_u32, "memory_mib" => 1024_u32);
        if i == 11 {
            error!(inst_log, "instance failed to start";
                   "reason" => "image not found");
        }
    }

    warn!(log, "disk near capacity";
          "disk" => "M.2_0", "free_pct" => 14_u32);
    warn!(log, "ntp drift exceeded threshold";
          "drift_ms" => 122_u32);
    info!(log, "ntp sync complete"; "drift_ms" => 3_u32);
    Ok(())
}

fn mgs_block(path: &Utf8Path) -> std::io::Result<()> {
    let log = open_drain(path, "MGS");
    info!(log, "MGS starting up");
    for switch in ["switch0", "switch1"] {
        let s_log = log.new(o!("switch" => switch));
        info!(s_log, "switch detected");
        info!(s_log, "ignition state read"; "powered" => true);
        debug!(s_log, "thermal sensor read"; "celsius" => 42_u32);
        debug!(s_log, "thermal sensor read"; "celsius" => 43_u32);
        debug!(s_log, "thermal sensor read"; "celsius" => 44_u32);
        warn!(s_log, "transceiver missing"; "port" => 7_u32);
    }
    error!(log, "switch port flapping";
           "switch" => "switch0", "port" => 12_u32);
    Ok(())
}

fn crdb_block(path: &Utf8Path) -> std::io::Result<()> {
    let log = open_drain(path, "CockroachDB");
    info!(log, "node startup");
    info!(log, "joined cluster"; "node_id" => 3_u32);
    for i in 0..15_u32 {
        debug!(log, "executed sql";
               "stmt" => "SELECT * FROM omicron.public.instance LIMIT 100",
               "elapsed_ms" => 5 + (i * 3) % 25);
    }
    warn!(log, "slow query";
          "stmt" =>
              "SELECT count(*) FROM omicron.public.network_interface",
          "elapsed_ms" => 1850_u32);
    error!(log, "transaction aborted";
           "reason" => "retry exhausted");
    Ok(())
}
