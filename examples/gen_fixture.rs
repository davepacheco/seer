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
        /// Records to write *per file*.  Each sled's file gets the
        /// same count; default is enough to exercise the merge under a
        /// realistic load.
        #[arg(short = 'n', long, default_value_t = 1000)]
        count: usize,
    },
}

fn main() -> io::Result<()> {
    match Args::parse().mode {
        Mode::Single { output } => write_single(&output),
        Mode::Multi { output_dir, count } => write_multi(&output_dir, count),
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
    let file =
        File::options().append(true).open(path).expect("open output file");
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

fn sled_agent_block(path: &Utf8Path, sled: &'static str) -> io::Result<()> {
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
    msg: String,
    /// Extra structured fields beyond the bunyan core.  Owned because
    /// the schedule mixes static keys with values computed per record
    /// (e.g. instance ids); keeping these as plain `String`/`Value`
    /// pairs avoids gymnastics for an example program.
    extras: Vec<(String, Value)>,
}

fn write_multi(dir: &Utf8Path, count: usize) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;

    // Three sleds with tightly-staggered start offsets so the merged
    // stream is visibly interleaved from the very first row, not just
    // after the slowest source's first event finally lands.  The sleds
    // share the same schedule shape — only the per-file `sled` and
    // `hostname` constants and the timestamp anchors differ — so the
    // merge produces a tight A/B/C cadence the reader can verify by
    // eye.
    let sleds = [
        SledSpec {
            sled: "sled-01",
            hostname: "oxz-sled-01.oxide.test",
            start_offset_s: 0,
        },
        SledSpec {
            sled: "sled-02",
            hostname: "oxz-sled-02.oxide.test",
            start_offset_s: 1,
        },
        SledSpec {
            sled: "sled-03",
            hostname: "oxz-sled-03.oxide.test",
            start_offset_s: 2,
        },
    ];

    let schedule = build_schedule(count);
    let mut total = 0;
    for spec in &sleds {
        let path = dir.join(format!("{}.log", spec.sled));
        let written = write_sled_log(&path, spec, &schedule)?;
        total += written;
        println!("wrote {written} records to {path}");
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
    let base = Utc.timestamp_opt(MULTI_EPOCH, 0).single().expect("valid epoch");
    for entry in schedule {
        let time =
            base + Duration::seconds(spec.start_offset_s + entry.delta_s);
        let mut extras = entry.extras.clone();
        // The two fields that distinguish files at a glance.
        extras.push(("sled".to_string(), Value::String(spec.sled.to_string())));
        write_record(
            &mut file,
            "SledAgent",
            spec.hostname,
            time,
            entry.level,
            &entry.msg,
            &extras,
        )?;
    }
    Ok(schedule.len())
}

/// Builds a sled-agent record schedule of exactly `count` entries.
///
/// The schedule starts with two boot-time records and then loops over
/// instance-lifecycle cycles (ensure → starting → running), peppered
/// with periodic warnings (disk capacity, NTP drift), follow-up infos
/// (NTP sync), and an occasional fault.  Time advances monotonically;
/// the spacing is deterministic so the merge across sleds produces
/// stable, visually-checkable interleavings.
///
/// The generator over-emits and then truncates to `count`, so callers
/// can request any size from 0 records up.
fn build_schedule(count: usize) -> Vec<ScheduledRecord> {
    use slog_levels::*;

    let mut records: Vec<ScheduledRecord> = Vec::with_capacity(count);
    let mut delta: i64 = 0;

    // Boot preamble.
    push_record(
        &mut records,
        &mut delta,
        0,
        INFO,
        "sled-agent starting up",
        vec![],
    );
    push_record(&mut records, &mut delta, 2, INFO, "rack initialized", vec![]);

    // Repeated instance lifecycles; periodic warns/errors interspersed.
    let mut inst: u32 = 0;
    while records.len() < count {
        let inst_id = format!("inst-{inst:04}");

        push_record(
            &mut records,
            &mut delta,
            3,
            DEBUG,
            "received instance ensure",
            vec![
                ("instance_id".to_string(), inst_id.clone().into()),
                ("kind".to_string(), "running".into()),
            ],
        );
        push_record(
            &mut records,
            &mut delta,
            1,
            INFO,
            "instance starting",
            vec![
                ("instance_id".to_string(), inst_id.clone().into()),
                ("image".to_string(), "alpine-3.19".into()),
            ],
        );
        push_record(
            &mut records,
            &mut delta,
            2,
            INFO,
            "instance running",
            vec![
                ("instance_id".to_string(), inst_id.clone().into()),
                ("vcpus".to_string(), 2.into()),
                ("memory_mib".to_string(), 1024.into()),
            ],
        );

        // Every 30th instance fails to start; provides occasional
        // ERROR rows for filtering exercises.
        if inst.is_multiple_of(30) && inst > 0 {
            push_record(
                &mut records,
                &mut delta,
                2,
                ERROR,
                "instance failed to start",
                vec![
                    ("instance_id".to_string(), inst_id.clone().into()),
                    ("reason".to_string(), "image not found".into()),
                ],
            );
        }

        // Every 25th instance triggers an NTP drift warn followed by a
        // sync-complete info, modelling a typical recovery pair.
        if inst.is_multiple_of(25) && inst > 0 {
            push_record(
                &mut records,
                &mut delta,
                4,
                WARN,
                "ntp drift exceeded threshold",
                vec![("drift_ms".to_string(), Value::from(122))],
            );
            push_record(
                &mut records,
                &mut delta,
                3,
                INFO,
                "ntp sync complete",
                vec![("drift_ms".to_string(), Value::from(2))],
            );
        }

        // Every 40th instance: disk-capacity warning.
        if inst.is_multiple_of(40) && inst > 0 {
            push_record(
                &mut records,
                &mut delta,
                5,
                WARN,
                "disk near capacity",
                vec![
                    ("disk".to_string(), "M.2_0".into()),
                    ("free_pct".to_string(), 14.into()),
                ],
            );
        }

        // Idle gap between instances so the timeline isn't packed.
        delta += 2;
        inst += 1;
    }

    records.truncate(count);
    records
}

/// Advances `delta` by `bump`, then appends a record at the new offset.
/// Helper to keep [`build_schedule`]'s call sites readable.
fn push_record(
    records: &mut Vec<ScheduledRecord>,
    delta: &mut i64,
    bump: i64,
    level: u8,
    msg: &str,
    extras: Vec<(String, Value)>,
) {
    *delta += bump;
    records.push(ScheduledRecord {
        delta_s: *delta,
        level,
        msg: msg.to_string(),
        extras,
    });
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
    extras: &[(String, Value)],
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
    let object = record.as_object_mut().expect("constructed object literal");
    for (k, v) in extras {
        object.insert(k.clone(), v.clone());
    }
    writeln!(file, "{record}")
}
