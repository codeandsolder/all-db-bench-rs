use super::{
    ContinuousNoiseArgs,
    proc::{
        ProcStat, ancestor_pids, clock_ticks_per_second, proc_io, proc_pids, proc_stat,
        uptime_seconds,
    },
};
use anyhow::{Context, Result, bail};
use num_traits::ToPrimitive;
use rustix::process::{Pid, Signal, getpriority_process, kill_process_group, setpriority_process};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::process::{CommandExt, ExitStatusExt},
    path::Path,
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

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
    fs::read_to_string(format!("/proc/{pid}/comm")).map_or_default(|text| text.trim().to_owned())
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

struct Snapshot {
    meta: ProcMeta,
    io: ProcIo,
    captured: Instant,
    born_since: u64,
}

impl Snapshot {
    fn capture(ticks: u64) -> Result<Self> {
        let born_since = (uptime_seconds()? * int_as_f64!(ticks))
            .to_u64()
            .context("invalid uptime in clock ticks")?;
        let mut meta = BTreeMap::new();
        let mut io = BTreeMap::new();
        for pid in proc_pids()? {
            let Some(stat) = proc_stat(pid) else { continue };
            let counters = proc_io(pid);
            // /proc can change PID ownership between the stat and io reads.
            let Some(current) =
                proc_stat(pid).filter(|current| current.start_ticks == stat.start_ticks)
            else {
                continue;
            };
            meta.insert(pid, current);
            if let Some(counters) = counters {
                io.insert((pid, current.start_ticks), counters);
            }
        }
        Ok(Self {
            meta,
            io,
            captured: Instant::now(),
            born_since,
        })
    }

    fn delta(&self, previous: &Self, pid: u32, stat: &ProcStat) -> Counters {
        let key = (pid, stat.start_ticks);
        let prior = previous
            .meta
            .get(&pid)
            .filter(|old| old.start_ticks == stat.start_ticks);
        let born = prior.is_none() && stat.start_ticks >= previous.born_since;
        let (read, write) = self.io.get(&key).map_or((0, 0), |&(read, write)| {
            previous.io.get(&key).map_or(
                if born { (read, write) } else { (0, 0) },
                |&(old_read, old_write)| {
                    (
                        read.saturating_sub(old_read),
                        write.saturating_sub(old_write),
                    )
                },
            )
        });
        let cpu = prior.map_or(if born { stat.cpu_ticks } else { 0 }, |old| {
            stat.cpu_ticks.saturating_sub(old.cpu_ticks)
        });
        Counters { read, write, cpu }
    }
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Counters {
    read: u64,
    write: u64,
    cpu: u64,
}

impl Counters {
    const fn io(self) -> u64 {
        self.read.saturating_add(self.write)
    }
    const fn add(&mut self, other: Self) {
        self.read = self.read.saturating_add(other.read);
        self.write = self.write.saturating_add(other.write);
        self.cpu = self.cpu.saturating_add(other.cpu);
    }
}

#[derive(Default)]
struct Attributed {
    counters: Counters,
    max_io_rate_bps: f64,
    command: String,
}

#[derive(Serialize)]
struct Offender {
    pid: u32,
    start_ticks: u64,
    command: String,
    read_bytes: u64,
    write_bytes: u64,
    io_bytes: u64,
    cpu_seconds: f64,
    max_io_rate_mib_s: f64,
}

struct Accounting {
    ticks: u64,
    previous: Snapshot,
    own_ancestors: BTreeSet<u32>,
    child_identities: BTreeSet<IoIdentity>,
    totals: Counters,
    max_io_rate: f64,
    max_cpu_percent: f64,
    samples: u64,
    attributed: BTreeMap<IoIdentity, Attributed>,
}

impl Accounting {
    fn new(ticks: u64) -> Result<Self> {
        Ok(Self {
            ticks,
            previous: Snapshot::capture(ticks)?,
            own_ancestors: ancestor_pids(std::process::id()),
            child_identities: BTreeSet::new(),
            totals: Counters::default(),
            max_io_rate: 0.0,
            max_cpu_percent: 0.0,
            samples: 0,
            attributed: BTreeMap::new(),
        })
    }

    fn record(&mut self, snapshot: Snapshot, child_root: u32, ignore_nice: i32) {
        let elapsed = snapshot
            .captured
            .duration_since(self.previous.captured)
            .as_secs_f64()
            .max(1e-9);
        // A child can be reparented between samples. The dedicated process group
        // and identities previously observed in its tree both retain ownership.
        let roots = snapshot
            .meta
            .iter()
            .filter_map(|(&pid, stat)| {
                (stat.pgrp == child_root
                    || self.child_identities.contains(&(pid, stat.start_ticks)))
                .then_some(pid)
            })
            .collect::<BTreeSet<_>>();
        let child_tree = continuous_descendants(&snapshot.meta, &roots);
        for pid in &child_tree {
            if let Some(stat) = snapshot.meta.get(pid) {
                self.child_identities.insert((*pid, stat.start_ticks));
            }
        }
        let mut interval = Counters::default();
        for (&pid, stat) in &snapshot.meta {
            if self.own_ancestors.contains(&pid)
                || child_tree.contains(&pid)
                || pid == 2
                || stat.ppid == 2
                || stat.nice >= ignore_nice
            {
                continue;
            }
            let delta = snapshot.delta(&self.previous, pid, stat);
            if delta == Counters::default() {
                continue;
            }
            interval.add(delta);
            let item = self
                .attributed
                .entry((pid, stat.start_ticks))
                .or_insert_with(|| Attributed {
                    command: proc_cmd(pid),
                    ..Attributed::default()
                });
            item.counters.add(delta);
            item.max_io_rate_bps = item.max_io_rate_bps.max(int_as_f64!(delta.io()) / elapsed);
        }
        self.totals.add(interval);
        self.max_io_rate = self.max_io_rate.max(int_as_f64!(interval.io()) / elapsed);
        self.max_cpu_percent = self
            .max_cpu_percent
            .max(100.0 * int_as_f64!(interval.cpu) / int_as_f64!(self.ticks) / elapsed);
        self.samples = self.samples.saturating_add(1);
        self.previous = snapshot;
    }

    fn top_processes(&self) -> Vec<Offender> {
        let mut top = self
            .attributed
            .iter()
            .map(|(&(pid, start_ticks), item)| Offender {
                pid,
                start_ticks,
                command: item.command.clone(),
                read_bytes: item.counters.read,
                write_bytes: item.counters.write,
                io_bytes: item.counters.io(),
                cpu_seconds: int_as_f64!(item.counters.cpu) / int_as_f64!(self.ticks),
                max_io_rate_mib_s: item.max_io_rate_bps / (1024.0 * 1024.0),
            })
            .collect::<Vec<_>>();
        top.sort_by(|left, right| {
            right
                .io_bytes
                .cmp(&left.io_bytes)
                .then_with(|| right.cpu_seconds.total_cmp(&left.cpu_seconds))
                .then_with(|| left.pid.cmp(&right.pid))
        });
        top.truncate(20);
        top
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
        .unwrap_or(1)
}

// Until the child is reaped, its process group cannot be reused.
struct MonitoredChild {
    child: Child,
    reaped: bool,
}

impl MonitoredChild {
    fn spawn(args: &[String]) -> Result<Self> {
        let (program, args) = args.split_first().context("command is required after --")?;
        let child = Command::new(program)
            .args(args)
            .process_group(0)
            .spawn()
            .with_context(|| format!("failed to spawn {program}"))?;
        Ok(Self {
            child,
            reaped: false,
        })
    }

    fn poll(&mut self) -> Result<Option<i32>> {
        let status = self
            .child
            .try_wait()
            .context("failed to poll benchmark child")?;
        self.reaped = status.is_some();
        Ok(status.map(exit_code))
    }
}

impl Drop for MonitoredChild {
    fn drop(&mut self) {
        if !self.reaped {
            if let Some(pid) = i32::try_from(self.child.id()).ok().and_then(Pid::from_raw) {
                let _ = kill_process_group(pid, Signal::KILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
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

#[derive(Serialize)]
struct MonitorReport<'a> {
    monitor_version: u32,
    command: &'a [String],
    child_pid: u32,
    child_returncode: i32,
    duration_s: f64,
    sample_ms: f64,
    sample_count: u64,
    monitor_nice_before: i32,
    monitor_nice: i32,
    ignore_nice_at_least: i32,
    max_foreign_cpu_percent: f64,
    max_foreign_io_average_mib_s: f64,
    max_foreign_io_rate_mib_s: f64,
    foreign_read_bytes: u64,
    foreign_write_bytes: u64,
    foreign_io_bytes: u64,
    foreign_io_average_mib_s: f64,
    peak_foreign_io_rate_mib_s: f64,
    peak_foreign_cpu_percent: f64,
    system_io_full_psi_fraction: f64,
    contaminated: bool,
    contamination_reasons: Vec<&'static str>,
    top_foreign_processes: Vec<Offender>,
    observe_only: bool,
}

fn validate(args: ContinuousNoiseArgs<'_>) -> Result<Duration> {
    if args.command.is_empty() {
        bail!("command is required after --");
    }
    if !args.sample_ms.is_finite() || args.sample_ms <= 0.0 {
        bail!("--sample-ms must be positive and finite");
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
    let period = Duration::try_from_secs_f64(args.sample_ms / 1000.0)
        .context("sample period out of range")?;
    if period.is_zero() {
        bail!("sample period rounds to zero");
    }
    Ok(period)
}

fn monitor(
    child: &mut MonitoredChild,
    accounting: &mut Accounting,
    period: Duration,
    ignore_nice: i32,
) -> Result<i32> {
    loop {
        let status = child.poll()?;
        let elapsed = accounting.previous.captured.elapsed();
        if status.is_none() && elapsed < period {
            thread::sleep(
                period
                    .saturating_sub(elapsed)
                    .min(Duration::from_millis(50)),
            );
            continue;
        }
        accounting.record(
            Snapshot::capture(accounting.ticks)?,
            child.child.id(),
            ignore_nice,
        );
        if let Some(code) = status {
            return Ok(code);
        }
    }
}

pub fn run_continuous(args: ContinuousNoiseArgs<'_>) -> Result<i32> {
    let period = validate(args)?;
    let monitor_nice_before = getpriority_process(None).context("getpriority failed")?;
    let mut accounting = Accounting::new(clock_ticks_per_second()?)?;
    let psi_start = psi_io_full_total_us();
    let started = Instant::now();
    let mut child = MonitoredChild::spawn(args.command)?;
    // Only the monitor, not the benchmark, inherits the lower priority.
    if monitor_nice_before < 15 {
        setpriority_process(None, 15).context("failed to lower monitor priority")?;
    }
    let monitor_nice = getpriority_process(None).context("getpriority after set failed")?;
    let child_rc = monitor(
        &mut child,
        &mut accounting,
        period,
        args.ignore_cpu_nice_at_least,
    )?;
    let duration = started.elapsed().as_secs_f64().max(1e-9);
    let psi_delta_us = psi_io_full_total_us().saturating_sub(psi_start);
    let total_io = accounting.totals.io();
    let average_io = int_as_f64!(total_io) / duration;
    let reasons = contamination_reasons(
        average_io,
        accounting.max_io_rate,
        accounting.max_cpu_percent,
        args.max_foreign_io_average_mib_s,
        args.max_foreign_io_rate_mib_s,
        args.max_foreign_cpu_percent,
    );
    let contaminated = !reasons.is_empty();
    let result = MonitorReport {
        monitor_version: 3,
        command: args.command,
        child_pid: child.child.id(),
        child_returncode: child_rc,
        duration_s: duration,
        sample_ms: args.sample_ms,
        sample_count: accounting.samples,
        monitor_nice_before,
        monitor_nice,
        ignore_nice_at_least: args.ignore_cpu_nice_at_least,
        max_foreign_cpu_percent: args.max_foreign_cpu_percent,
        max_foreign_io_average_mib_s: args.max_foreign_io_average_mib_s,
        max_foreign_io_rate_mib_s: args.max_foreign_io_rate_mib_s,
        foreign_read_bytes: accounting.totals.read,
        foreign_write_bytes: accounting.totals.write,
        foreign_io_bytes: total_io,
        foreign_io_average_mib_s: average_io / (1024.0 * 1024.0),
        peak_foreign_io_rate_mib_s: accounting.max_io_rate / (1024.0 * 1024.0),
        peak_foreign_cpu_percent: accounting.max_cpu_percent,
        system_io_full_psi_fraction: int_as_f64!(psi_delta_us) / 1e6 / duration,
        contaminated,
        contamination_reasons: reasons,
        top_foreign_processes: accounting.top_processes(),
        observe_only: args.observe_only,
    };
    atomic_write_json(args.json_out, &result)?;
    Ok(if child_rc != 0 {
        child_rc
    } else if contaminated && !args.observe_only {
        75
    } else {
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(start_ticks: u64, ppid: u32, pgrp: u32, cpu_ticks: u64) -> ProcStat {
        ProcStat {
            start_ticks,
            ppid,
            pgrp,
            cpu_ticks,
            nice: 0,
        }
    }

    #[test]
    fn accounting_handles_pid_reuse_reparenting_and_new_processes() {
        let start = Instant::now();
        let previous = Snapshot {
            meta: BTreeMap::from([(20, stat(10, 1, 20, 100)), (30, stat(10, 1, 30, 90))]),
            io: BTreeMap::from([((20, 10), (100, 200)), ((30, 10), (300, 400))]),
            captured: start,
            born_since: 20,
        };
        let mut accounting = Accounting {
            ticks: 100,
            previous,
            own_ancestors: BTreeSet::new(),
            child_identities: BTreeSet::new(),
            totals: Counters::default(),
            max_io_rate: 0.0,
            max_cpu_percent: 0.0,
            samples: 0,
            attributed: BTreeMap::new(),
        };
        accounting.record(
            Snapshot {
                meta: BTreeMap::from([
                    (20, stat(10, 1, 20, 150)),
                    (30, stat(21, 1, 30, 25)),
                    (40, stat(22, 1, 10, 1000)),
                    (41, stat(23, 40, 41, 1000)),
                    (50, stat(5, 1, 50, 999)),
                ]),
                io: BTreeMap::from([
                    ((20, 10), (120, 230)),
                    ((30, 21), (11, 12)),
                    ((40, 22), (9000, 9000)),
                    ((50, 5), (9000, 9000)),
                ]),
                captured: start + Duration::from_secs(1),
                born_since: 120,
            },
            10,
            15,
        );
        assert_eq!(
            accounting.totals,
            Counters {
                read: 31,
                write: 42,
                cpu: 75
            }
        );
        assert_eq!(
            accounting.child_identities,
            BTreeSet::from([(40, 22), (41, 23)])
        );
        accounting.record(
            Snapshot {
                meta: BTreeMap::from([
                    (41, stat(23, 1, 41, 2000)),
                    (42, stat(130, 41, 42, 2000)),
                    (40, stat(130, 1, 40, 50)),
                ]),
                io: BTreeMap::from([((41, 23), (99_999, 99_999)), ((40, 130), (1, 2))]),
                captured: start + Duration::from_secs(2),
                born_since: 220,
            },
            10,
            15,
        );
        assert_eq!(
            accounting.totals,
            Counters {
                read: 32,
                write: 44,
                cpu: 125
            }
        );
        assert_eq!(accounting.samples, 2);
    }

    #[test]
    fn invalid_sample_duration_is_rejected_before_spawn() {
        let command = ["true".to_owned()];
        let args = ContinuousNoiseArgs {
            json_out: Path::new("/dev/null"),
            sample_ms: f64::MAX,
            ignore_cpu_nice_at_least: 15,
            max_foreign_cpu_percent: 50.0,
            max_foreign_io_average_mib_s: 2.0,
            max_foreign_io_rate_mib_s: 8.0,
            observe_only: true,
            command: &command,
        };
        assert!(validate(args).is_err());
    }

    #[test]
    fn child_is_reaped_on_guard_drop() -> Result<()> {
        let child = MonitoredChild::spawn(&["sleep".into(), "60".into()])?;
        let pid = child.child.id();
        drop(child);
        assert!(proc_stat(pid).is_none());
        Ok(())
    }
}
