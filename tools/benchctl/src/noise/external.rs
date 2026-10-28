use super::{
    ExternalNoiseArgs,
    proc::{ancestor_pids, clock_ticks_per_second, proc_pids, uptime_seconds},
};
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    process::Command,
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
pub(super) struct ProcessRow {
    pub(super) pid: u32,
    pub(super) ppid: u32,
    pub(super) nice: i32,
    pub(super) cpu_percent: f64,
    pub(super) comm: String,
    pub(super) args: String,
}

pub(super) fn is_kernel_thread(row: &ProcessRow) -> bool {
    row.pid == 2 || row.ppid == 2 || (row.args.starts_with('[') && row.args.ends_with(']'))
}

pub(super) fn classify_process(
    row: &ProcessRow,
    ignore_nice_at_least: i32,
) -> Option<&'static str> {
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

pub(super) fn cpu_percent_from_samples(
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

pub(super) fn descendant_pids(rows: &[ProcessRow], roots: &BTreeSet<u32>) -> BTreeSet<u32> {
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

pub(super) fn low_priority_sccache_tree(rows: &[ProcessRow], min_nice: i32) -> BTreeSet<u32> {
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

pub(super) fn aggregate_foreign_cpu_percent(
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
