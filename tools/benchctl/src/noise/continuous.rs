use super::{
    ContinuousNoiseArgs,
    proc::{ancestor_pids, clock_ticks_per_second, proc_pids, uptime_seconds},
};
use anyhow::{Context, Result, bail};
use rustix::process::{Pid, Signal, getpriority_process, kill_process_group, setpriority_process};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::process::{CommandExt, ExitStatusExt},
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProcStat {
    pub(super) ppid: u32,
    pub(super) nice: i32,
    pub(super) cpu_ticks: u64,
    pub(super) start_ticks: u64,
}

fn proc_stat(pid: u32) -> Option<ProcStat> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = raw.rfind(')')?;
    let fields = raw.get(close + 2..)?.split_whitespace().collect::<Vec<_>>();
    Some(ProcStat {
        ppid: fields.get(1)?.parse().ok()?,
        nice: fields.get(16)?.parse().ok()?,
        cpu_ticks: fields
            .get(11)?
            .parse::<u64>()
            .ok()?
            .saturating_add(fields.get(12)?.parse::<u64>().ok()?),
        start_ticks: fields.get(19)?.parse().ok()?,
    })
}

fn proc_io(pid: u32) -> Option<(u64, u64)> {
    let text = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let mut read_bytes = 0_u64;
    let mut write_bytes = 0_u64;
    for line in text.lines() {
        let (key, value) = line.split_once(':')?;
        let parsed = value.trim().parse::<u64>().ok()?;
        match key {
            "read_bytes" => read_bytes = parsed,
            "write_bytes" => write_bytes = parsed,
            _ => {}
        }
    }
    Some((read_bytes, write_bytes))
}

fn proc_cmd(pid: u32) -> String {
    if let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) {
        let bytes = raw
            .into_iter()
            .map(|byte| if byte == 0 { b' ' } else { byte })
            .collect::<Vec<_>>();
        let text = String::from_utf8_lossy(&bytes).trim().to_owned();
        if !text.is_empty() {
            return text;
        }
    }
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|text| text.trim().to_owned())
        .unwrap_or_default()
}

pub(super) fn continuous_descendants(
    meta: &BTreeMap<u32, ProcStat>,
    roots: &BTreeSet<u32>,
) -> BTreeSet<u32> {
    let mut out = roots.clone();
    loop {
        let mut changed = false;
        for (&pid, stat) in meta {
            if !out.contains(&pid) && out.contains(&stat.ppid) {
                out.insert(pid);
                changed = true;
            }
        }
        if !changed {
            return out;
        }
    }
}

type IoIdentity = (u32, u64);
type ProcMeta = BTreeMap<u32, ProcStat>;
type ProcIo = BTreeMap<IoIdentity, (u64, u64)>;

fn sample_proc() -> (ProcMeta, ProcIo) {
    let mut meta = BTreeMap::new();
    let mut io = BTreeMap::new();
    for pid in proc_pids() {
        let Some(stat) = proc_stat(pid) else {
            continue;
        };
        meta.insert(pid, stat);
        if let Some(counters) = proc_io(pid) {
            io.insert((pid, stat.start_ticks), counters);
        }
    }
    (meta, io)
}

fn psi_io_full_total_us() -> u64 {
    let Ok(text) = fs::read_to_string("/proc/pressure/io") else {
        return 0;
    };
    for line in text.lines() {
        if !line.starts_with("full ") {
            continue;
        }
        for part in line.split_whitespace() {
            if let Some(value) = part.strip_prefix("total=")
                && let Ok(value) = value.parse::<u64>()
            {
                return value;
            }
        }
    }
    0
}

pub(super) fn contamination_reasons(
    average_io_rate_bytes_s: f64,
    peak_io_rate_bytes_s: f64,
    peak_cpu_percent: f64,
    max_io_average_mib_s: f64,
    max_io_rate_mib_s: f64,
    max_cpu_percent: f64,
) -> Vec<&'static str> {
    let mut reasons = Vec::new();
    if average_io_rate_bytes_s > max_io_average_mib_s * 1024.0 * 1024.0 {
        reasons.push("foreign-io-average");
    }
    if peak_io_rate_bytes_s > max_io_rate_mib_s * 1024.0 * 1024.0 {
        reasons.push("foreign-io-rate");
    }
    if peak_cpu_percent >= max_cpu_percent {
        reasons.push("foreign-cpu");
    }
    reasons
}

#[derive(Debug, Default, Clone)]
struct Attributed {
    read_bytes: u64,
    write_bytes: u64,
    cpu_ticks: u64,
    max_io_rate_bps: f64,
    command: String,
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
        .unwrap_or(1)
}

fn terminate_child_group(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let raw_pid = i32::try_from(child.id()).ok();
    if let Some(pid) = raw_pid.and_then(Pid::from_raw) {
        let _ = kill_process_group(pid, Signal::TERM);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = kill_process_group(pid, Signal::KILL);
    }
    let _ = child.wait();
}

fn atomic_write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map_or_else(|| "tmp".to_owned(), |value| format!("{value}.tmp"));
    let tmp = path.with_extension(extension);
    fs::write(&tmp, serde_json::to_string_pretty(value)? + "\n")
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

pub(crate) fn run_continuous(args: ContinuousNoiseArgs<'_>) -> Result<i32> {
    if args.command.is_empty() {
        bail!("command is required after --");
    }
    if !args.sample_ms.is_finite() || args.sample_ms <= 0.0 {
        bail!("--sample-ms must be positive");
    }
    for (name, value) in [
        ("--max-foreign-cpu-percent", args.max_foreign_cpu_percent),
        (
            "--max-foreign-io-average-mib-s",
            args.max_foreign_io_average_mib_s,
        ),
        (
            "--max-foreign-io-rate-mib-s",
            args.max_foreign_io_rate_mib_s,
        ),
    ] {
        if !value.is_finite() || value < 0.0 {
            bail!("{name} must be non-negative and finite");
        }
    }

    let ticks = clock_ticks_per_second()?;
    let start_uptime_s = uptime_seconds();
    let start_ticks = (start_uptime_s * ticks as f64) as u64;
    let own_ancestors = ancestor_pids(std::process::id());
    let (meta_prev, mut io_prev) = sample_proc();
    let mut cpu_prev = meta_prev
        .iter()
        .map(|(&pid, stat)| ((pid, stat.start_ticks), stat.cpu_ticks))
        .collect::<BTreeMap<_, _>>();
    let psi_start = psi_io_full_total_us();

    let started = Instant::now();
    let mut command = Command::new(&args.command[0]);
    command.args(&args.command[1..]);
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {:?}", args.command))?;
    let child_root = child.id();

    let monitor_nice_before = getpriority_process(None).context("getpriority failed")?;
    if monitor_nice_before < 15 {
        setpriority_process(None, 15).context("failed to lower monitor priority")?;
    }
    let monitor_nice = getpriority_process(None).context("getpriority after set failed")?;

    let monitor_result = (|| -> Result<(i32, Value)> {
        let mut total_read = 0_u64;
        let mut total_write = 0_u64;
        let mut max_io_rate = 0.0_f64;
        let mut max_cpu_percent = 0.0_f64;
        let mut samples = 0_u64;
        let mut attributed = BTreeMap::<IoIdentity, Attributed>::new();
        let mut child_identities = BTreeSet::<IoIdentity>::new();
        let mut last_sample = started;
        let child_rc;

        loop {
            let status = child.try_wait().context("failed to poll benchmark child")?;
            let now = Instant::now();
            let sample_period = Duration::from_secs_f64(args.sample_ms / 1000.0);
            let since_last = now.saturating_duration_since(last_sample);
            if status.is_none() && since_last < sample_period {
                thread::sleep((sample_period - since_last).min(Duration::from_millis(50)));
                continue;
            }

            let (meta, io_now) = sample_proc();
            let elapsed = last_sample.elapsed().as_secs_f64().max(1e-9);
            let child_tree = continuous_descendants(&meta, &BTreeSet::from([child_root]));
            for pid in &child_tree {
                if let Some(stat) = meta.get(pid) {
                    child_identities.insert((*pid, stat.start_ticks));
                }
            }
            let excluded = own_ancestors
                .union(&child_tree)
                .copied()
                .chain(std::iter::once(std::process::id()))
                .collect::<BTreeSet<_>>();
            let mut interval_read = 0_u64;
            let mut interval_write = 0_u64;
            let mut interval_cpu_ticks = 0_u64;

            for (&pid, stat) in &meta {
                let key = (pid, stat.start_ticks);
                if excluded.contains(&pid)
                    || child_identities.contains(&key)
                    || pid == 2
                    || stat.ppid == 2
                {
                    continue;
                }

                if stat.nice < args.ignore_cpu_nice_at_least
                    && let Some(&current_io) = io_now.get(&key)
                {
                    let (dr, dw) = if let Some(&previous_io) = io_prev.get(&key) {
                        (
                            current_io.0.saturating_sub(previous_io.0),
                            current_io.1.saturating_sub(previous_io.1),
                        )
                    } else if stat.start_ticks >= start_ticks {
                        current_io
                    } else {
                        (0, 0)
                    };
                    if dr != 0 || dw != 0 {
                        interval_read = interval_read.saturating_add(dr);
                        interval_write = interval_write.saturating_add(dw);
                        let item = attributed.entry(key).or_default();
                        if item.command.is_empty() {
                            item.command = proc_cmd(pid);
                        }
                        item.read_bytes = item.read_bytes.saturating_add(dr);
                        item.write_bytes = item.write_bytes.saturating_add(dw);
                        item.max_io_rate_bps = item.max_io_rate_bps.max((dr + dw) as f64 / elapsed);
                    }
                }

                let dcpu = if let Some(&previous_cpu) = cpu_prev.get(&key) {
                    stat.cpu_ticks.saturating_sub(previous_cpu)
                } else if stat.start_ticks >= start_ticks {
                    stat.cpu_ticks
                } else {
                    0
                };
                if stat.nice < args.ignore_cpu_nice_at_least && dcpu != 0 {
                    interval_cpu_ticks = interval_cpu_ticks.saturating_add(dcpu);
                    let item = attributed.entry(key).or_default();
                    if item.command.is_empty() {
                        item.command = proc_cmd(pid);
                    }
                    item.cpu_ticks = item.cpu_ticks.saturating_add(dcpu);
                }
            }

            total_read = total_read.saturating_add(interval_read);
            total_write = total_write.saturating_add(interval_write);
            max_io_rate = max_io_rate.max((interval_read + interval_write) as f64 / elapsed);
            max_cpu_percent =
                max_cpu_percent.max(100.0 * (interval_cpu_ticks as f64 / ticks as f64) / elapsed);
            samples = samples.saturating_add(1);
            io_prev = io_now;
            cpu_prev = meta
                .iter()
                .map(|(&pid, stat)| ((pid, stat.start_ticks), stat.cpu_ticks))
                .collect();
            last_sample = Instant::now();

            if let Some(status) = status {
                child_rc = exit_code(status);
                break;
            }
        }

        let duration = started.elapsed().as_secs_f64().max(1e-9);
        let psi_delta_us = psi_io_full_total_us().saturating_sub(psi_start);
        let mut top = attributed
            .into_iter()
            .filter_map(|((pid, proc_start), item)| {
                let io_bytes = item.read_bytes.saturating_add(item.write_bytes);
                let cpu_seconds = item.cpu_ticks as f64 / ticks as f64;
                if io_bytes == 0 && cpu_seconds == 0.0 {
                    return None;
                }
                let command = if item.command.is_empty() {
                    proc_cmd(pid)
                } else {
                    item.command
                };
                Some(json!({
                    "pid": pid,
                    "start_ticks": proc_start,
                    "command": command,
                    "read_bytes": item.read_bytes,
                    "write_bytes": item.write_bytes,
                    "io_bytes": io_bytes,
                    "cpu_seconds": cpu_seconds,
                    "max_io_rate_mib_s": item.max_io_rate_bps / (1024.0 * 1024.0),
                }))
            })
            .collect::<Vec<_>>();
        top.sort_by(|left, right| {
            right["io_bytes"]
                .as_u64()
                .cmp(&left["io_bytes"].as_u64())
                .then_with(|| {
                    right["cpu_seconds"]
                        .as_f64()
                        .unwrap_or(0.0)
                        .total_cmp(&left["cpu_seconds"].as_f64().unwrap_or(0.0))
                })
        });
        top.truncate(20);

        let total_io = total_read.saturating_add(total_write);
        let reasons = contamination_reasons(
            total_io as f64 / duration,
            max_io_rate,
            max_cpu_percent,
            args.max_foreign_io_average_mib_s,
            args.max_foreign_io_rate_mib_s,
            args.max_foreign_cpu_percent,
        );
        let result = json!({
            "monitor_version": 2,
            "command": args.command,
            "child_pid": child_root,
            "child_returncode": child_rc,
            "duration_s": duration,
            "sample_ms": args.sample_ms,
            "sample_count": samples,
            "monitor_nice_before": monitor_nice_before,
            "monitor_nice": monitor_nice,
            "ignore_nice_at_least": args.ignore_cpu_nice_at_least,
            "max_foreign_cpu_percent": args.max_foreign_cpu_percent,
            "max_foreign_io_average_mib_s": args.max_foreign_io_average_mib_s,
            "max_foreign_io_rate_mib_s": args.max_foreign_io_rate_mib_s,
            "foreign_read_bytes": total_read,
            "foreign_write_bytes": total_write,
            "foreign_io_bytes": total_io,
            "foreign_io_average_mib_s": total_io as f64 / duration / (1024.0 * 1024.0),
            "peak_foreign_io_rate_mib_s": max_io_rate / (1024.0 * 1024.0),
            "peak_foreign_cpu_percent": max_cpu_percent,
            "system_io_full_psi_fraction": psi_delta_us as f64 / 1e6 / duration,
            "contaminated": !reasons.is_empty(),
            "contamination_reasons": reasons,
            "top_foreign_processes": top,
            "observe_only": args.observe_only,
        });
        Ok((child_rc, result))
    })();

    let (child_rc, result) = match monitor_result {
        Ok(value) => value,
        Err(error) => {
            terminate_child_group(&mut child);
            return Err(error);
        }
    };
    atomic_write_json(args.json_out, &result)?;
    if child_rc != 0 {
        return Ok(child_rc);
    }
    let contaminated = result["contaminated"].as_bool().unwrap_or(false);
    Ok(if contaminated && !args.observe_only {
        75
    } else {
        0
    })
}
