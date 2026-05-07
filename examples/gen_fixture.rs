// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generates sample bunyan log files used for manual testing of `seer`
//! and `seeit`.
//!
//! Two modes:
//!
//! - `single` — writes one file with a hand-built mix of components
//!   (Nexus, sled-agent, MGS, CockroachDB) at real wall-clock times.
//!   Useful for general filter/render exercises.
//! - `multi` — writes several sled-agent files with *overlapping*
//!   deterministic timestamps and per-file constant fields (`sled`,
//!   `hostname`).  Aimed at exercising the engine's cross-source merge:
//!   point `seer`/`seeit` at the directory and verify by eye that the
//!   `sled` field alternates between sources as time advances.
//!
//! ```text
//! cargo run --example gen_fixture -- single tests/fixtures/sample.log
//! cargo run --example gen_fixture -- multi  tests/fixtures/multi
//! ```

use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, Duration, TimeZone, Utc};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use slog::{Drain, Logger, debug, error, info, o, warn};
use std::fs::File;
use std::io::{self, Write};
use std::sync::Mutex;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand)]
enum Mode {
    /// Write a single file with a hand-built mix of components, using
    /// the system clock for timestamps.
    Single {
        /// Output path; truncated if it already exists.
        #[arg(default_value = "tests/fixtures/sample.log")]
        output: Utf8PathBuf,
    },
    /// Write multiple sled-agent files with overlapping deterministic
    /// timestamps and per-file constant `sled`/`hostname` fields.
    Multi {
        /// Output directory; created if it does not exist.  Existing
        /// `*.log` files in this directory are truncated.
        #[arg(default_value = "tests/fixtures/multi")]
        output_dir: Utf8PathBuf,
    },
}

fn main() -> io::Result<()> {
    match Args::parse().mode {
        Mode::Single { output } => write_single(&output),
        Mode::Multi { output_dir } => write_multi(&output_dir),
    }
}

// ---------- single-file mode ----------

fn write_single(path: &Utf8Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    File::create(path)?; // truncate

    nexus_block(path)?;
    sled_agent_block(path, "sled-01")?;
    sled_agent_block(path, "sled-02")?;
    sled_agent_block(path, "sled-03")?;
    mgs_block(path)?;
    crdb_block(path)?;

    let count = std::fs::read_to_string(path)?.lines().count();
    println!("wrote {count} records to {path}");
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

fn nexus_block(path: &Utf8Path) -> io::Result<()> {
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
) -> io::Result<()> {
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

fn mgs_block(path: &Utf8Path) -> io::Result<()> {
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

fn crdb_block(path: &Utf8Path) -> io::Result<()> {
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

// ---------- multi-file mode ----------

/// Wall-clock anchor for multi-file timestamps.  An arbitrary fixed
/// instant; chosen to be far from the present so the records are
/// obviously synthetic when displayed.
const MULTI_EPOCH: i64 = 1_710_000_000; // 2024-03-09 16:00:00 UTC

/// Per-sled input to [`write_sled_log`]: which sled's log this is and
/// how many seconds after [`MULTI_EPOCH`] its first record sits at.
/// Different start offsets are what produce overlapping ranges across
/// the resulting files.
struct SledSpec {
    sled: &'static str,
    /// Used as the bunyan `hostname` field.  Distinct from `sled` so a
    /// reader can verify that *both* per-file constants travel through
    /// the merge unchanged.
    hostname: &'static str,
    /// Seconds after [`MULTI_EPOCH`] at which this sled's first event
    /// occurs.  Each subsequent event's offset comes from the schedule.
    start_offset_s: i64,
}

/// One entry in the per-sled record schedule.  All sleds share this
/// same shape; only the absolute timestamp (offset by `start_offset_s`)
/// and per-file fields (`sled`, `hostname`) differ between files, which
/// is what makes the merged stream easy to spot-check by eye.
struct ScheduledRecord {
    /// Offset in seconds from this sled's `start_offset_s`.
    delta_s: i64,
    level: u8,
    msg: &'static str,
    /// Extra structured fields beyond the bunyan core.  Built per-call
    /// so the schedule stays a `'static` table.
    extras: fn() -> Vec<(&'static str, Value)>,
}

fn write_multi(dir: &Utf8Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;

    // Three sleds with start offsets that interleave: every ~10s window
    // contains records from two or three different sleds.  Verify by
    // eye that the `sled` field weaves through the merged output.
    let sleds = [
        SledSpec {
            sled: "sled-01",
            hostname: "oxz-sled-01.oxide.test",
            start_offset_s: 0,
        },
        SledSpec {
            sled: "sled-02",
            hostname: "oxz-sled-02.oxide.test",
            start_offset_s: 8,
        },
        SledSpec {
            sled: "sled-03",
            hostname: "oxz-sled-03.oxide.test",
            start_offset_s: 16,
        },
    ];

    let schedule = sled_schedule();
    let mut total = 0;
    for spec in &sleds {
        let path = dir.join(format!("{}.log", spec.sled));
        let count = write_sled_log(&path, spec, &schedule)?;
        total += count;
        println!("wrote {count} records to {path}");
    }
    println!("total: {total} records across {} files", sleds.len());
    Ok(())
}

fn write_sled_log(
    path: &Utf8Path,
    spec: &SledSpec,
    schedule: &[ScheduledRecord],
) -> io::Result<usize> {
    let mut file = File::create(path)?;
    let base =
        Utc.timestamp_opt(MULTI_EPOCH, 0).single().expect("valid epoch");
    for entry in schedule {
        let time = base
            + Duration::seconds(spec.start_offset_s + entry.delta_s);
        let mut extras = (entry.extras)();
        // The two fields that distinguish files at a glance.
        extras.push(("sled", Value::String(spec.sled.to_string())));
        write_record(
            &mut file,
            "SledAgent",
            spec.hostname,
            time,
            entry.level,
            entry.msg,
            extras,
        )?;
    }
    Ok(schedule.len())
}

/// Common per-sled record schedule.  Every sled's file goes through the
/// same sequence of operations; what differs between files is the
/// timestamp anchor (so files overlap in time) and the per-file
/// `sled` / `hostname` constants (so the merged output is easy to
/// verify by eye).
fn sled_schedule() -> Vec<ScheduledRecord> {
    use slog_levels::*;
    vec![
        ScheduledRecord {
            delta_s: 0,
            level: INFO,
            msg: "sled-agent starting up",
            extras: || vec![],
        },
        ScheduledRecord {
            delta_s: 2,
            level: INFO,
            msg: "rack initialized",
            extras: || vec![],
        },
        ScheduledRecord {
            delta_s: 5,
            level: DEBUG,
            msg: "received instance ensure",
            extras: || {
                vec![
                    ("instance_id", "inst-0001".into()),
                    ("kind", "running".into()),
                ]
            },
        },
        ScheduledRecord {
            delta_s: 6,
            level: INFO,
            msg: "instance starting",
            extras: || {
                vec![
                    ("instance_id", "inst-0001".into()),
                    ("image", "alpine-3.19".into()),
                ]
            },
        },
        ScheduledRecord {
            delta_s: 8,
            level: INFO,
            msg: "instance running",
            extras: || {
                vec![
                    ("instance_id", "inst-0001".into()),
                    ("vcpus", 2.into()),
                    ("memory_mib", 1024.into()),
                ]
            },
        },
        ScheduledRecord {
            delta_s: 15,
            level: INFO,
            msg: "ntp sync complete",
            extras: || vec![("drift_ms", 3.into())],
        },
        ScheduledRecord {
            delta_s: 20,
            level: WARN,
            msg: "disk near capacity",
            extras: || {
                vec![
                    ("disk", "M.2_0".into()),
                    ("free_pct", 14.into()),
                ]
            },
        },
        ScheduledRecord {
            delta_s: 26,
            level: DEBUG,
            msg: "received instance ensure",
            extras: || {
                vec![
                    ("instance_id", "inst-0002".into()),
                    ("kind", "running".into()),
                ]
            },
        },
        ScheduledRecord {
            delta_s: 27,
            level: INFO,
            msg: "instance starting",
            extras: || {
                vec![
                    ("instance_id", "inst-0002".into()),
                    ("image", "alpine-3.19".into()),
                ]
            },
        },
        ScheduledRecord {
            delta_s: 30,
            level: ERROR,
            msg: "instance failed to start",
            extras: || {
                vec![
                    ("instance_id", "inst-0002".into()),
                    ("reason", "image not found".into()),
                ]
            },
        },
        ScheduledRecord {
            delta_s: 38,
            level: INFO,
            msg: "ntp sync complete",
            extras: || vec![("drift_ms", 1.into())],
        },
    ]
}

mod slog_levels {
    pub(super) const DEBUG: u8 = 20;
    pub(super) const INFO: u8 = 30;
    pub(super) const WARN: u8 = 40;
    pub(super) const ERROR: u8 = 50;
}

fn write_record(
    file: &mut impl Write,
    name: &str,
    hostname: &str,
    time: DateTime<Utc>,
    level: u8,
    msg: &str,
    extras: Vec<(&str, Value)>,
) -> io::Result<()> {
    let mut record = json!({
        "v": 0,
        "level": level,
        "name": name,
        "hostname": hostname,
        "pid": 1234,
        "time": time.to_rfc3339(),
        "msg": msg,
    });
    let object =
        record.as_object_mut().expect("constructed object literal");
    for (k, v) in extras {
        object.insert(k.to_string(), v);
    }
    writeln!(file, "{record}")
}
