use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs,
    process::Command,
    sync::OnceLock,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProcSnapshot {
    pub cpu_runtime_ns: u64,
    pub runqueue_wait_ns: u64,
    pub timeslices: u64,
    pub minflt: u64,
    pub majflt: u64,
    pub voluntary_ctx_switches: u64,
    pub involuntary_ctx_switches: u64,
    pub rchar: u64,
    pub wchar: u64,
    pub syscr: u64,
    pub syscw: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cancelled_write_bytes: u64,
    pub current_rss_kib: u64,
    pub threads: u64,
    #[serde(skip)]
    task_counters: BTreeMap<u32, TaskCounters>,
    #[serde(skip)]
    process_cpu_ticks: u64,
    #[serde(skip)]
    clock_ticks_per_second: u64,
    #[serde(skip)]
    captured_at: Option<Instant>,
}

#[derive(Clone, Debug, Default)]
struct TaskCounters {
    cpu_runtime_ns: u64,
    runqueue_wait_ns: u64,
    timeslices: u64,
    voluntary_ctx_switches: u64,
    involuntary_ctx_switches: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProcDelta {
    pub cpu_runtime_ns: u64,
    pub process_cpu_tick_runtime_ns: u64,
    pub process_cpu_tick_minus_task_ns: u64,
    pub runqueue_wait_ns: u64,
    pub timeslices: u64,
    pub minflt: u64,
    pub majflt: u64,
    pub voluntary_ctx_switches: u64,
    pub involuntary_ctx_switches: u64,
    pub rchar: u64,
    pub wchar: u64,
    pub syscr: u64,
    pub syscw: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cancelled_write_bytes: u64,
    pub rss_before_kib: u64,
    pub rss_after_kib: u64,
    pub threads_before: u64,
    pub threads_after: u64,
    pub cpu_runtime_fraction_of_wall: f64,
    pub runqueue_wait_fraction_of_wall: f64,
    pub accounting_wall_ns: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SystemSnapshot {
    pub load1: f64,
    pub mem_available_kib: u64,
    pub swap_free_kib: u64,
    pub dirty_kib: u64,
    pub writeback_kib: u64,
    pub psi_cpu_some_us: u64,
    pub psi_io_some_us: u64,
    pub psi_io_full_us: u64,
    pub psi_memory_some_us: u64,
    pub psi_memory_full_us: u64,
    pub pgfault: u64,
    pub pgmajfault: u64,
    pub pswpin: u64,
    pub pswpout: u64,
    pub pgscan_direct: u64,
    pub pgscan_kswapd: u64,
    #[serde(skip)]
    captured_at: Option<Instant>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SystemDelta {
    pub psi_cpu_some_us: u64,
    pub psi_io_some_us: u64,
    pub psi_io_full_us: u64,
    pub psi_memory_some_us: u64,
    pub psi_memory_full_us: u64,
    pub pgfault: u64,
    pub pgmajfault: u64,
    pub pswpin: u64,
    pub pswpout: u64,
    pub pgscan_direct: u64,
    pub pgscan_kswapd: u64,
    pub accounting_wall_ns: u64,
}

fn parse_colon_numbers(path: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if let Some(value) = rest
            .split_whitespace()
            .find_map(|token| token.parse::<u64>().ok())
        {
            out.insert(key.trim().to_string(), value);
        }
    }
    out
}

fn parse_space_numbers(path: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(key) = fields.next() else {
            continue;
        };
        if let Some(value) = fields.find_map(|token| token.parse::<u64>().ok()) {
            out.insert(key.to_string(), value);
        }
    }
    out
}

fn pressure_total(path: &str, class: &str) -> u64 {
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some(class) {
            continue;
        }
        for field in fields {
            if let Some(raw) = field.strip_prefix("total=") {
                if let Ok(value) = raw.parse() {
                    return value;
                }
            }
        }
    }
    0
}

fn saturating_delta(after: u64, before: u64) -> u64 {
    after.saturating_sub(before)
}

fn clock_ticks_per_second() -> u64 {
    static CLK_TCK: OnceLock<u64> = OnceLock::new();
    *CLK_TCK.get_or_init(|| {
        Command::new("getconf")
            .arg("CLK_TCK")
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .and_then(|text| text.trim().parse::<u64>().ok())
            .filter(|ticks| *ticks > 0)
            .unwrap_or(100)
    })
}

fn task_delta<F>(
    before: &BTreeMap<u32, TaskCounters>,
    after: &BTreeMap<u32, TaskCounters>,
    field: F,
) -> u64
where
    F: Fn(&TaskCounters) -> u64,
{
    after.iter().fold(0u64, |total, (tid, current)| {
        let delta = before.get(tid).map_or_else(
            || field(current),
            |previous| field(current).saturating_sub(field(previous)),
        );
        total.saturating_add(delta)
    })
}

impl ProcSnapshot {
    pub fn capture() -> Self {
        let status = parse_colon_numbers("/proc/self/status");
        let io = parse_colon_numbers("/proc/self/io");
        let clock_ticks_per_second = clock_ticks_per_second();

        // /proc/self/stat is process-wide for faults and utime/stime. The latter
        // is coarse (USER_HZ), so keep it as a monotonic backstop for workers
        // that may disappear between snapshots. Per-task schedstat supplies
        // nanosecond-resolution CPU/runqueue counters for surviving/new tasks.
        let (minflt, majflt, process_cpu_ticks) = match fs::read_to_string("/proc/self/stat") {
            Ok(text) => {
                let tail = text.rsplit_once(") ").map(|(_, tail)| tail).unwrap_or("");
                let fields: Vec<&str> = tail.split_whitespace().collect();
                let minflt = fields.get(7).and_then(|v| v.parse().ok()).unwrap_or(0);
                let majflt = fields.get(9).and_then(|v| v.parse().ok()).unwrap_or(0);
                let utime: u64 = fields.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
                let stime: u64 = fields.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
                (minflt, majflt, utime.saturating_add(stime))
            }
            Err(_) => (0, 0, 0),
        };

        let mut task_counters = BTreeMap::new();
        if let Ok(tasks) = fs::read_dir("/proc/self/task") {
            for task in tasks.flatten() {
                let Some(tid) = task
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u32>().ok())
                else {
                    continue;
                };
                let base = task.path();
                let (cpu_runtime_ns, runqueue_wait_ns, timeslices) =
                    match fs::read_to_string(base.join("schedstat")) {
                        Ok(text) => {
                            let mut fields = text.split_whitespace();
                            (
                                fields.next().and_then(|v| v.parse().ok()).unwrap_or(0),
                                fields.next().and_then(|v| v.parse().ok()).unwrap_or(0),
                                fields.next().and_then(|v| v.parse().ok()).unwrap_or(0),
                            )
                        }
                        Err(_) => (0, 0, 0),
                    };
                let task_status = parse_colon_numbers(&base.join("status").to_string_lossy());
                task_counters.insert(
                    tid,
                    TaskCounters {
                        cpu_runtime_ns,
                        runqueue_wait_ns,
                        timeslices,
                        voluntary_ctx_switches: *task_status
                            .get("voluntary_ctxt_switches")
                            .unwrap_or(&0),
                        involuntary_ctx_switches: *task_status
                            .get("nonvoluntary_ctxt_switches")
                            .unwrap_or(&0),
                    },
                );
            }
        }

        let cpu_runtime_ns = task_counters
            .values()
            .fold(0u64, |sum, task| sum.saturating_add(task.cpu_runtime_ns));
        let runqueue_wait_ns = task_counters
            .values()
            .fold(0u64, |sum, task| sum.saturating_add(task.runqueue_wait_ns));
        let timeslices = task_counters
            .values()
            .fold(0u64, |sum, task| sum.saturating_add(task.timeslices));
        let voluntary_ctx_switches = task_counters.values().fold(0u64, |sum, task| {
            sum.saturating_add(task.voluntary_ctx_switches)
        });
        let involuntary_ctx_switches = task_counters.values().fold(0u64, |sum, task| {
            sum.saturating_add(task.involuntary_ctx_switches)
        });

        Self {
            cpu_runtime_ns,
            runqueue_wait_ns,
            timeslices,
            minflt,
            majflt,
            voluntary_ctx_switches,
            involuntary_ctx_switches,
            rchar: *io.get("rchar").unwrap_or(&0),
            wchar: *io.get("wchar").unwrap_or(&0),
            syscr: *io.get("syscr").unwrap_or(&0),
            syscw: *io.get("syscw").unwrap_or(&0),
            read_bytes: *io.get("read_bytes").unwrap_or(&0),
            write_bytes: *io.get("write_bytes").unwrap_or(&0),
            cancelled_write_bytes: *io.get("cancelled_write_bytes").unwrap_or(&0),
            current_rss_kib: *status.get("VmRSS").unwrap_or(&0),
            threads: (task_counters.len() as u64).max(*status.get("Threads").unwrap_or(&0)),
            task_counters,
            process_cpu_ticks,
            clock_ticks_per_second,
            captured_at: Some(Instant::now()),
        }
    }

    pub fn delta(&self, after: &Self, elapsed: Duration) -> ProcDelta {
        let accounting_elapsed = self
            .captured_at
            .zip(after.captured_at)
            .and_then(|(before, after)| after.checked_duration_since(before))
            .unwrap_or(elapsed);
        let accounting_wall_ns = accounting_elapsed.as_nanos().max(1).min(u64::MAX as u128) as u64;
        let wall_ns = accounting_wall_ns as f64;

        let task_cpu_runtime_ns = task_delta(&self.task_counters, &after.task_counters, |task| {
            task.cpu_runtime_ns
        });
        let tick_delta = saturating_delta(after.process_cpu_ticks, self.process_cpu_ticks);
        let ticks_per_second = after
            .clock_ticks_per_second
            .max(self.clock_ticks_per_second)
            .max(1);
        let process_cpu_runtime_ns = ((tick_delta as u128) * 1_000_000_000u128
            / ticks_per_second as u128)
            .min(u64::MAX as u128) as u64;
        // schedstat is nanosecond-resolution and is the reported CPU metric.
        // The process-wide USER_HZ counter remains diagnostic only: taking
        // max(task_ns, tick_ns) badly overestimates very short runs because one
        // 10 ms tick can dwarf a millisecond-scale benchmark interval.
        let cpu_runtime_ns = task_cpu_runtime_ns;
        let process_cpu_tick_minus_task_ns =
            process_cpu_runtime_ns.saturating_sub(task_cpu_runtime_ns);
        let runqueue_wait_ns = task_delta(&self.task_counters, &after.task_counters, |task| {
            task.runqueue_wait_ns
        });
        let timeslices = task_delta(&self.task_counters, &after.task_counters, |task| {
            task.timeslices
        });
        let voluntary_ctx_switches =
            task_delta(&self.task_counters, &after.task_counters, |task| {
                task.voluntary_ctx_switches
            });
        let involuntary_ctx_switches =
            task_delta(&self.task_counters, &after.task_counters, |task| {
                task.involuntary_ctx_switches
            });

        ProcDelta {
            cpu_runtime_ns,
            process_cpu_tick_runtime_ns: process_cpu_runtime_ns,
            process_cpu_tick_minus_task_ns,
            runqueue_wait_ns,
            timeslices,
            minflt: saturating_delta(after.minflt, self.minflt),
            majflt: saturating_delta(after.majflt, self.majflt),
            voluntary_ctx_switches,
            involuntary_ctx_switches,
            rchar: saturating_delta(after.rchar, self.rchar),
            wchar: saturating_delta(after.wchar, self.wchar),
            syscr: saturating_delta(after.syscr, self.syscr),
            syscw: saturating_delta(after.syscw, self.syscw),
            read_bytes: saturating_delta(after.read_bytes, self.read_bytes),
            write_bytes: saturating_delta(after.write_bytes, self.write_bytes),
            cancelled_write_bytes: saturating_delta(
                after.cancelled_write_bytes,
                self.cancelled_write_bytes,
            ),
            rss_before_kib: self.current_rss_kib,
            rss_after_kib: after.current_rss_kib,
            threads_before: self.threads,
            threads_after: after.threads,
            cpu_runtime_fraction_of_wall: cpu_runtime_ns as f64 / wall_ns,
            runqueue_wait_fraction_of_wall: runqueue_wait_ns as f64 / wall_ns,
            accounting_wall_ns,
        }
    }
}

impl SystemSnapshot {
    pub fn capture() -> Self {
        let mem = parse_colon_numbers("/proc/meminfo");
        let vm = parse_space_numbers("/proc/vmstat");
        let load1 = fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|text| text.split_whitespace().next()?.parse().ok())
            .unwrap_or(0.0);
        Self {
            load1,
            mem_available_kib: *mem.get("MemAvailable").unwrap_or(&0),
            swap_free_kib: *mem.get("SwapFree").unwrap_or(&0),
            dirty_kib: *mem.get("Dirty").unwrap_or(&0),
            writeback_kib: *mem.get("Writeback").unwrap_or(&0),
            psi_cpu_some_us: pressure_total("/proc/pressure/cpu", "some"),
            psi_io_some_us: pressure_total("/proc/pressure/io", "some"),
            psi_io_full_us: pressure_total("/proc/pressure/io", "full"),
            psi_memory_some_us: pressure_total("/proc/pressure/memory", "some"),
            psi_memory_full_us: pressure_total("/proc/pressure/memory", "full"),
            pgfault: *vm.get("pgfault").unwrap_or(&0),
            pgmajfault: *vm.get("pgmajfault").unwrap_or(&0),
            pswpin: *vm.get("pswpin").unwrap_or(&0),
            pswpout: *vm.get("pswpout").unwrap_or(&0),
            pgscan_direct: *vm.get("pgscan_direct").unwrap_or(&0),
            pgscan_kswapd: *vm.get("pgscan_kswapd").unwrap_or(&0),
            captured_at: Some(Instant::now()),
        }
    }

    pub fn delta(&self, after: &Self) -> SystemDelta {
        let accounting_wall_ns = self
            .captured_at
            .zip(after.captured_at)
            .and_then(|(before, after)| after.checked_duration_since(before))
            .map(|elapsed| elapsed.as_nanos().max(1).min(u64::MAX as u128) as u64)
            .unwrap_or(1);
        SystemDelta {
            psi_cpu_some_us: saturating_delta(after.psi_cpu_some_us, self.psi_cpu_some_us),
            psi_io_some_us: saturating_delta(after.psi_io_some_us, self.psi_io_some_us),
            psi_io_full_us: saturating_delta(after.psi_io_full_us, self.psi_io_full_us),
            psi_memory_some_us: saturating_delta(after.psi_memory_some_us, self.psi_memory_some_us),
            psi_memory_full_us: saturating_delta(after.psi_memory_full_us, self.psi_memory_full_us),
            pgfault: saturating_delta(after.pgfault, self.pgfault),
            pgmajfault: saturating_delta(after.pgmajfault, self.pgmajfault),
            pswpin: saturating_delta(after.pswpin, self.pswpin),
            pswpout: saturating_delta(after.pswpout, self.pswpout),
            pgscan_direct: saturating_delta(after.pgscan_direct, self.pgscan_direct),
            pgscan_kswapd: saturating_delta(after.pgscan_kswapd, self.pgscan_kswapd),
            accounting_wall_ns,
        }
    }
}
