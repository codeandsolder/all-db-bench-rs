use anyhow::{Context, Result, bail};
use nix::unistd::{SysconfVar, sysconf};
use regex::Regex;
use rustix::process::{Pid, Signal, getpriority_process, kill_process_group, setpriority_process};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::process::{CommandExt, ExitStatusExt},
    path::Path,
    process::{Child, Command, ExitStatus},
    sync::LazyLock,
    thread,
    time::{Duration, Instant},
};

static BUILD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[ /])(?:cargo(?:-[A-Za-z0-9_.-]+)?|rustc|clippy-driver|cc1plus|clang(?:\+\+)?|gcc|g\+\+|cmake|ninja|make)(?:\s|$)")
        .unwrap_or_else(|error| unreachable!("static BUILD_RE is invalid: {error}"))
});
static WIDE_SCAN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|\s)(?:find|rg|ripgrep)\s+(?:/root\b|/srv\b|/opt\b|/home\b|/mnt\b)")
        .unwrap_or_else(|error| unreachable!("static WIDE_SCAN_RE is invalid: {error}"))
});
static ALLOWED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:run-io-contention-matrix\.sh|fio --name=pressure-(?:read|write))")
        .unwrap_or_else(|error| unreachable!("static ALLOWED_RE is invalid: {error}"))
});
static SCCACHE_WORKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|/)sccache-dist server(?:\s|$)")
        .unwrap_or_else(|error| unreachable!("static SCCACHE_WORKER_RE is invalid: {error}"))
});

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ProcessRow {
    pid: u32,
    ppid: u32,
    nice: i32,
    cpu_percent: f64,
    comm: String,
    args: String,
}

#[derive(Debug)]
pub(crate) struct ExternalNoiseArgs<'a> {
    pub(crate) ignore_nice_at_least: i32,
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) exclude_pids: &'a [u32],
    pub(crate) max_aggregate_cpu_percent: f64,
    pub(crate) sample_ms: f64,
}

#[derive(Debug)]
pub(crate) struct ContinuousNoiseArgs<'a> {
    pub(crate) json_out: &'a Path,
    pub(crate) sample_ms: f64,
    pub(crate) ignore_cpu_nice_at_least: i32,
    pub(crate) max_foreign_cpu_percent: f64,
    pub(crate) max_foreign_io_average_mib_s: f64,
    pub(crate) max_foreign_io_rate_mib_s: f64,
    pub(crate) observe_only: bool,
    pub(crate) command: &'a [String],
}

fn is_kernel_thread(row: &ProcessRow) -> bool {
    row.pid == 2 || row.ppid == 2 || (row.args.starts_with('[') && row.args.ends_with(']'))
}

fn classify_process(row: &ProcessRow, ignore_nice_at_least: i32) -> Option<&'static str> {
    if is_kernel_thread(row)
        || row.nice >= ignore_nice_at_least
        || row.args.is_empty()
        || ALLOWED_RE.is_match(&row.args)
    {
        return None;
    }
    if WIDE_SCAN_RE.is_match(&row.args) && row.cpu_percent >= 1.0 {
        return Some("wide-filesystem-scan");
    }
    if BUILD_RE.is_match(&row.args) && row.cpu_percent >= 2.0 {
        return Some("compiler-or-build");
    }
    if row.cpu_percent >= 50.0 {
        return Some("foreign-high-cpu");
    }
    None
}

fn parse_proc_cpu(pid: u32) -> Option<(u64, u64)> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = raw.rfind(')')?;
    let fields = raw.get(close + 2..)?.split_whitespace().collect::<Vec<_>>();
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    let start = fields.get(19)?.parse::<u64>().ok()?;
    Some((utime.saturating_add(stime), start))
}

fn cpu_percent_from_samples(
    before: Option<(u64, u64)>,
    after: (u64, u64),
    elapsed_s: f64,
    uptime_s: f64,
    ticks_per_second: u64,
) -> f64 {
    let (after_ticks, after_start) = after;
    if let Some((before_ticks, before_start)) = before
        && before_start == after_start
    {
        let cpu_s = after_ticks.saturating_sub(before_ticks) as f64 / ticks_per_second as f64;
        return 100.0 * cpu_s / elapsed_s.max(1e-9);
    }
    let age_s = (uptime_s - after_start as f64 / ticks_per_second as f64)
        .max(1.0 / ticks_per_second as f64);
    100.0 * (after_ticks as f64 / ticks_per_second as f64) / age_s
}

fn proc_pids() -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .collect()
}

type MetadataRow = (u32, u32, i32, String, String);

fn metadata_rows() -> Result<Vec<MetadataRow>> {
    let output = Command::new("ps")
        .args(["-eo", "pid=,ppid=,ni=,comm=,args="])
        .output()
        .context("failed to run ps")?;
    if !output.status.success() {
        bail!(
            "ps failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout).context("ps output was not UTF-8")?;
    let mut rows = Vec::new();
    for raw in stdout.lines() {
        let mut parts = raw.split_whitespace();
        let (Some(pid), Some(ppid), Some(nice), Some(comm)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(ppid), Ok(nice)) =
            (pid.parse::<u32>(), ppid.parse::<u32>(), nice.parse::<i32>())
        else {
            continue;
        };
        let args = parts.collect::<Vec<_>>().join(" ");
        if args.is_empty() {
            continue;
        }
        rows.push((pid, ppid, nice, comm.to_owned(), args));
    }
    Ok(rows)
}

fn clock_ticks_per_second() -> Result<u64> {
    let ticks = sysconf(SysconfVar::CLK_TCK)
        .context("sysconf CLK_TCK failed")?
        .context("CLK_TCK unavailable")?;
    u64::try_from(ticks).context("CLK_TCK was non-positive")
}

fn uptime_seconds() -> f64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn process_rows(sample_seconds: f64) -> Result<Vec<ProcessRow>> {
    let before = proc_pids()
        .into_iter()
        .filter_map(|pid| parse_proc_cpu(pid).map(|stat| (pid, stat)))
        .collect::<BTreeMap<_, _>>();
    let started = Instant::now();
    thread::sleep(Duration::from_secs_f64(sample_seconds.max(0.0)));
    let elapsed = started.elapsed().as_secs_f64().max(1e-9);
    let uptime_s = uptime_seconds();
    let ticks = clock_ticks_per_second()?;
    let mut rows = Vec::new();
    for (pid, ppid, nice, comm, args) in metadata_rows()? {
        let Some(after) = parse_proc_cpu(pid) else {
            continue;
        };
        rows.push(ProcessRow {
            pid,
            ppid,
            nice,
            cpu_percent: cpu_percent_from_samples(
                before.get(&pid).copied(),
                after,
                elapsed,
                uptime_s,
                ticks,
            ),
            comm,
            args,
        });
    }
    Ok(rows)
}

fn proc_ppid(pid: u32) -> Option<u32> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = raw.rfind(')')?;
    raw.get(close + 2..)?
        .split_whitespace()
        .nth(1)?
        .parse::<u32>()
        .ok()
}

fn ancestor_pids(pid: u32) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    let mut current = pid;
    while current > 1 && !out.contains(&current) {
        out.insert(current);
        let Some(parent) = proc_ppid(current) else {
            break;
        };
        current = parent;
    }
    out.insert(1);
    out
}

fn descendant_pids(rows: &[ProcessRow], roots: &BTreeSet<u32>) -> BTreeSet<u32> {
    let mut descendants = roots.clone();
    loop {
        let mut changed = false;
        for row in rows {
            if !descendants.contains(&row.pid) && descendants.contains(&row.ppid) {
                descendants.insert(row.pid);
                changed = true;
            }
        }
        if !changed {
            return descendants;
        }
    }
}

fn low_priority_sccache_tree(rows: &[ProcessRow], min_nice: i32) -> BTreeSet<u32> {
    let roots = rows
        .iter()
        .filter(|row| row.nice >= min_nice && SCCACHE_WORKER_RE.is_match(&row.args))
        .map(|row| row.pid)
        .collect::<BTreeSet<_>>();
    if roots.is_empty() {
        BTreeSet::new()
    } else {
        descendant_pids(rows, &roots)
    }
}

fn aggregate_foreign_cpu_percent(
    rows: &[ProcessRow],
    excluded_pids: &BTreeSet<u32>,
    ignore_nice_at_least: i32,
) -> f64 {
    rows.iter()
        .filter(|row| {
            !excluded_pids.contains(&row.pid)
                && !is_kernel_thread(row)
                && row.nice < ignore_nice_at_least
                && !row.args.is_empty()
                && !ALLOWED_RE.is_match(&row.args)
        })
        .map(|row| row.cpu_percent)
        .sum()
}

pub(crate) fn run_external(args: ExternalNoiseArgs<'_>) -> Result<i32> {
    if !args.sample_ms.is_finite() || !args.max_aggregate_cpu_percent.is_finite() {
        bail!("noise thresholds must be finite");
    }
    let self_pid = std::process::id();
    let ancestors = ancestor_pids(self_pid);
    let rows = process_rows(args.sample_ms.max(0.0) / 1000.0)?;
    let own_tree = descendant_pids(&rows, &BTreeSet::from([self_pid]));
    let sccache_tree = low_priority_sccache_tree(&rows, 10);
    let explicit_roots = args.exclude_pids.iter().copied().collect::<BTreeSet<_>>();
    let explicit_tree = if explicit_roots.is_empty() {
        BTreeSet::new()
    } else {
        descendant_pids(&rows, &explicit_roots)
    };
    let excluded = ancestors
        .union(&own_tree)
        .copied()
        .collect::<BTreeSet<_>>()
        .union(&sccache_tree)
        .copied()
        .collect::<BTreeSet<_>>()
        .union(&explicit_tree)
        .copied()
        .collect::<BTreeSet<_>>();

    let mut offenders = rows
        .iter()
        .filter(|row| !excluded.contains(&row.pid))
        .filter_map(|row| {
            let reason = classify_process(row, args.ignore_nice_at_least)?;
            let mut value = serde_json::to_value(row).ok()?;
            value["reason"] = Value::String(reason.to_owned());
            Some(value)
        })
        .collect::<Vec<_>>();
    offenders.sort_by(|left, right| {
        let left_cpu = left["cpu_percent"].as_f64().unwrap_or(0.0);
        let right_cpu = right["cpu_percent"].as_f64().unwrap_or(0.0);
        right_cpu
            .total_cmp(&left_cpu)
            .then_with(|| left["pid"].as_u64().cmp(&right["pid"].as_u64()))
    });
    let aggregate_cpu_percent =
        aggregate_foreign_cpu_percent(&rows, &excluded, args.ignore_nice_at_least);
    let aggregate_cpu_exceeded = aggregate_cpu_percent >= args.max_aggregate_cpu_percent;
    let kernel_thread_cpu_percent = rows
        .iter()
        .filter(|row| !excluded.contains(&row.pid) && is_kernel_thread(row))
        .map(|row| row.cpu_percent)
        .sum::<f64>();
    let quiet = offenders.is_empty() && !aggregate_cpu_exceeded;
    let result = json!({
        "quiet": quiet,
        "cpu_sample_ms": args.sample_ms,
        "ignore_nice_at_least": args.ignore_nice_at_least,
        "max_aggregate_cpu_percent": args.max_aggregate_cpu_percent,
        "aggregate_foreign_cpu_percent": aggregate_cpu_percent,
        "aggregate_cpu_exceeded": aggregate_cpu_exceeded,
        "kernel_thread_cpu_percent": kernel_thread_cpu_percent,
        "excluded_pids": explicit_tree,
        "offenders": offenders,
    });
    let encoded = serde_json::to_string(&result)? + "\n";
    if let Some(path) = args.json_out {
        fs::write(path, encoded).with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        print!("{encoded}");
    }
    Ok(if quiet { 0 } else { 75 })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcStat {
    ppid: u32,
    nice: i32,
    cpu_ticks: u64,
    start_ticks: u64,
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

fn continuous_descendants(meta: &BTreeMap<u32, ProcStat>, roots: &BTreeSet<u32>) -> BTreeSet<u32> {
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

fn contamination_reasons(
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

#[cfg(test)]
mod tests {
    use super::{
        ProcessRow, aggregate_foreign_cpu_percent, classify_process, contamination_reasons,
        continuous_descendants, cpu_percent_from_samples, descendant_pids, is_kernel_thread,
        low_priority_sccache_tree,
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn row(nice: i32, cpu: f64, comm: &str, args: &str) -> ProcessRow {
        ProcessRow {
            pid: 123,
            ppid: 1,
            nice,
            cpu_percent: cpu,
            comm: comm.to_owned(),
            args: args.to_owned(),
        }
    }

    #[test]
    fn external_classification_matches_policy() {
        assert_eq!(
            classify_process(&row(19, 95.0, "rustc", "rustc crate.rs"), 15),
            None
        );
        assert_eq!(
            classify_process(
                &row(0, 5.0, "clippy-driver", "clippy-driver rustc src/lib.rs"),
                15
            ),
            Some("compiler-or-build")
        );
        assert_eq!(
            classify_process(
                &row(
                    0,
                    5.0,
                    "cargo-clippy",
                    "/root/.rustup/toolchains/nightly/bin/cargo-clippy clippy --fix"
                ),
                15
            ),
            Some("compiler-or-build")
        );
        assert_eq!(
            classify_process(
                &row(0, 2.0, "find", "find /root /srv /opt /home /mnt -type f"),
                15
            ),
            Some("wide-filesystem-scan")
        );
        assert_eq!(
            classify_process(&row(0, 75.0, "python3", "python3 worker.py"), 15),
            Some("foreign-high-cpu")
        );
        assert_eq!(
            classify_process(&row(0, 3.0, "tailscaled", "/usr/sbin/tailscaled"), 15),
            None
        );
    }

    #[test]
    fn kernel_threads_are_ignored() {
        let kernel = ProcessRow {
            pid: 900,
            ppid: 2,
            nice: -19,
            cpu_percent: 120.0,
            comm: "z_wr_iss".to_owned(),
            args: "[z_wr_iss]".to_owned(),
        };
        assert!(is_kernel_thread(&kernel));
        assert_eq!(classify_process(&kernel, 15), None);
        let user = ProcessRow {
            pid: 901,
            ppid: 1,
            nice: 0,
            cpu_percent: 20.0,
            comm: "python".to_owned(),
            args: "python worker.py".to_owned(),
        };
        assert_eq!(
            aggregate_foreign_cpu_percent(&[kernel, user], &BTreeSet::new(), 15),
            20.0
        );
    }

    #[test]
    fn descendant_and_sccache_trees_close_transitively() {
        let rows = vec![
            ProcessRow {
                pid: 200,
                ppid: 100,
                nice: 0,
                cpu_percent: 100.0,
                comm: "ps".into(),
                args: "ps".into(),
            },
            ProcessRow {
                pid: 201,
                ppid: 200,
                nice: 0,
                cpu_percent: 100.0,
                comm: "helper".into(),
                args: "helper child".into(),
            },
            ProcessRow {
                pid: 400,
                ppid: 1,
                nice: 10,
                cpu_percent: 20.0,
                comm: "sccache-dist".into(),
                args: "/usr/local/bin/sccache-dist server --config x".into(),
            },
            ProcessRow {
                pid: 401,
                ppid: 400,
                nice: 10,
                cpu_percent: 100.0,
                comm: "cc1plus".into(),
                args: "/usr/lib/gcc/cc1plus file.cc".into(),
            },
        ];
        assert_eq!(
            descendant_pids(&rows, &BTreeSet::from([100])),
            BTreeSet::from([100, 200, 201])
        );
        assert_eq!(
            low_priority_sccache_tree(&rows, 10),
            BTreeSet::from([400, 401])
        );
    }

    #[test]
    fn aggregate_cpu_catches_many_medium_workers() {
        let rows = vec![
            ProcessRow {
                pid: 800,
                ppid: 1,
                nice: 0,
                cpu_percent: 20.0,
                comm: "python".into(),
                args: "python a.py".into(),
            },
            ProcessRow {
                pid: 801,
                ppid: 1,
                nice: 0,
                cpu_percent: 20.0,
                comm: "python".into(),
                args: "python b.py".into(),
            },
            ProcessRow {
                pid: 802,
                ppid: 1,
                nice: 0,
                cpu_percent: 20.0,
                comm: "python".into(),
                args: "python c.py".into(),
            },
            ProcessRow {
                pid: 803,
                ppid: 1,
                nice: 19,
                cpu_percent: 95.0,
                comm: "python".into(),
                args: "python low.py".into(),
            },
            ProcessRow {
                pid: 804,
                ppid: 1,
                nice: 0,
                cpu_percent: 100.0,
                comm: "kvbench".into(),
                args: "/tmp/kvbench --engine redb".into(),
            },
        ];
        assert_eq!(
            aggregate_foreign_cpu_percent(&rows, &BTreeSet::new(), 15),
            160.0
        );
    }

    #[test]
    fn cpu_percent_uses_interval_and_handles_new_process() {
        assert!(
            (cpu_percent_from_samples(Some((10_000, 123)), (10_025, 123), 0.25, 1000.0, 100)
                - 100.0)
                .abs()
                < 1e-9
        );
        assert!(
            (cpu_percent_from_samples(None, (25, 99_975), 0.25, 1000.0, 100) - 100.0).abs() < 1e-9
        );
    }

    #[test]
    fn continuous_descendants_close_over_tree() {
        let meta = BTreeMap::from([
            (
                10,
                super::ProcStat {
                    ppid: 1,
                    nice: 0,
                    cpu_ticks: 0,
                    start_ticks: 100,
                },
            ),
            (
                11,
                super::ProcStat {
                    ppid: 10,
                    nice: 0,
                    cpu_ticks: 0,
                    start_ticks: 101,
                },
            ),
            (
                12,
                super::ProcStat {
                    ppid: 11,
                    nice: 0,
                    cpu_ticks: 0,
                    start_ticks: 102,
                },
            ),
            (
                20,
                super::ProcStat {
                    ppid: 1,
                    nice: 0,
                    cpu_ticks: 0,
                    start_ticks: 200,
                },
            ),
        ]);
        assert_eq!(
            continuous_descendants(&meta, &BTreeSet::from([10])),
            BTreeSet::from([10, 11, 12])
        );
    }

    #[test]
    fn contamination_thresholds_match_python_contract() {
        assert!(contamination_reasons(1024.0, 1024.0, 10.0, 1.0, 1.0, 50.0).is_empty());
        assert_eq!(
            contamination_reasons(
                2.0 * 1024.0 * 1024.0,
                2.0 * 1024.0 * 1024.0,
                50.0,
                1.0,
                1.0,
                50.0,
            ),
            ["foreign-io-average", "foreign-io-rate", "foreign-cpu"]
        );
    }
}
