use serde::Serialize;
use std::{collections::BTreeMap, fs, time::Duration};

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
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProcDelta {
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
    pub rss_before_kib: u64,
    pub rss_after_kib: u64,
    pub threads_before: u64,
    pub threads_after: u64,
    pub cpu_runtime_fraction_of_wall: f64,
    pub runqueue_wait_fraction_of_wall: f64,
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

impl ProcSnapshot {
    pub fn capture() -> Self {
        let status = parse_colon_numbers("/proc/self/status");
        let io = parse_colon_numbers("/proc/self/io");

        let (cpu_runtime_ns, runqueue_wait_ns, timeslices) =
            match fs::read_to_string("/proc/self/schedstat") {
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

        let (minflt, majflt) = match fs::read_to_string("/proc/self/stat") {
            Ok(text) => {
                let tail = text.rsplit_once(") ").map(|(_, tail)| tail).unwrap_or("");
                let fields: Vec<&str> = tail.split_whitespace().collect();
                (
                    fields.get(7).and_then(|v| v.parse().ok()).unwrap_or(0),
                    fields.get(9).and_then(|v| v.parse().ok()).unwrap_or(0),
                )
            }
            Err(_) => (0, 0),
        };

        Self {
            cpu_runtime_ns,
            runqueue_wait_ns,
            timeslices,
            minflt,
            majflt,
            voluntary_ctx_switches: *status.get("voluntary_ctxt_switches").unwrap_or(&0),
            involuntary_ctx_switches: *status.get("nonvoluntary_ctxt_switches").unwrap_or(&0),
            rchar: *io.get("rchar").unwrap_or(&0),
            wchar: *io.get("wchar").unwrap_or(&0),
            syscr: *io.get("syscr").unwrap_or(&0),
            syscw: *io.get("syscw").unwrap_or(&0),
            read_bytes: *io.get("read_bytes").unwrap_or(&0),
            write_bytes: *io.get("write_bytes").unwrap_or(&0),
            cancelled_write_bytes: *io.get("cancelled_write_bytes").unwrap_or(&0),
            current_rss_kib: *status.get("VmRSS").unwrap_or(&0),
            threads: *status.get("Threads").unwrap_or(&0),
        }
    }

    pub fn delta(&self, after: &Self, elapsed: Duration) -> ProcDelta {
        let wall_ns = elapsed.as_nanos().max(1) as f64;
        let cpu_runtime_ns = saturating_delta(after.cpu_runtime_ns, self.cpu_runtime_ns);
        let runqueue_wait_ns = saturating_delta(after.runqueue_wait_ns, self.runqueue_wait_ns);
        ProcDelta {
            cpu_runtime_ns,
            runqueue_wait_ns,
            timeslices: saturating_delta(after.timeslices, self.timeslices),
            minflt: saturating_delta(after.minflt, self.minflt),
            majflt: saturating_delta(after.majflt, self.majflt),
            voluntary_ctx_switches: saturating_delta(
                after.voluntary_ctx_switches,
                self.voluntary_ctx_switches,
            ),
            involuntary_ctx_switches: saturating_delta(
                after.involuntary_ctx_switches,
                self.involuntary_ctx_switches,
            ),
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
        }
    }

    pub fn delta(&self, after: &Self) -> SystemDelta {
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
        }
    }
}
