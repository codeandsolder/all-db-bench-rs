use anyhow::{Context, Result};
use nix::time::{ClockId, clock_gettime};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(crate) struct ProcessTreeArgs<'a> {
    pub(crate) pid: u32,
    pub(crate) output: Option<&'a Path>,
}

fn parse_stat(pid: u32) -> Option<(u32, u32)> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, tail) = text.rsplit_once(") ")?;
    let mut fields = tail.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse::<u32>().ok()?;
    Some((pid, ppid))
}

fn descendants(root: u32) -> Result<Vec<u32>> {
    let mut parents = BTreeMap::<u32, u32>::new();
    for entry in fs::read_dir("/proc").context("failed to list /proc")? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if let Some((pid, ppid)) = parse_stat(pid) {
            parents.insert(pid, ppid);
        }
    }

    let mut out = BTreeSet::from([root]);
    loop {
        let mut changed = false;
        for (&pid, &ppid) in &parents {
            if out.contains(&ppid) && out.insert(pid) {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    Ok(out
        .into_iter()
        .filter(|pid| PathBuf::from(format!("/proc/{pid}")).exists())
        .collect())
}

fn kv_file(path: &Path) -> BTreeMap<String, i64> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some((key, raw)) = line.split_once(':') else {
            continue;
        };
        let Some(token) = raw.split_whitespace().next() else {
            continue;
        };
        if let Ok(value) = token.parse::<i64>() {
            out.insert(key.to_owned(), value);
        }
    }
    out
}

fn monotonic_ns() -> Result<u64> {
    let now =
        clock_gettime(ClockId::CLOCK_MONOTONIC).context("clock_gettime(CLOCK_MONOTONIC) failed")?;
    let seconds = u64::try_from(now.tv_sec()).context("negative monotonic seconds")?;
    let nanos = u64::try_from(now.tv_nsec()).context("negative monotonic nanoseconds")?;
    Ok(seconds.saturating_mul(1_000_000_000).saturating_add(nanos))
}

fn snapshot(root: u32) -> Result<Value> {
    let pids = descendants(root)?;
    let mut cpu = 0_i64;
    let mut runqueue = 0_i64;
    let mut timeslices = 0_i64;
    let mut rss_kib = 0_i64;
    let mut peak_rss_kib = 0_i64;
    let mut threads = 0_i64;
    let io_keys = [
        "rchar",
        "wchar",
        "syscr",
        "syscw",
        "read_bytes",
        "write_bytes",
        "cancelled_write_bytes",
    ];
    let mut io_totals = BTreeMap::<&str, i64>::new();
    for key in io_keys {
        io_totals.insert(key, 0);
    }
    let mut live_pids = Vec::new();

    for pid in pids {
        let base = PathBuf::from(format!("/proc/{pid}"));
        if !base.exists() {
            continue;
        }
        live_pids.push(pid);
        let status = kv_file(&base.join("status"));
        rss_kib += status.get("VmRSS").copied().unwrap_or(0);
        peak_rss_kib += status.get("VmHWM").copied().unwrap_or(0);
        threads += status.get("Threads").copied().unwrap_or(0);
        let process_io = kv_file(&base.join("io"));
        for key in io_keys {
            if let Some(total) = io_totals.get_mut(key) {
                *total += process_io.get(key).copied().unwrap_or(0);
            }
        }
        let Ok(tasks) = fs::read_dir(base.join("task")) else {
            continue;
        };
        for task in tasks {
            let Ok(task) = task else { continue };
            let Ok(text) = fs::read_to_string(task.path().join("schedstat")) else {
                continue;
            };
            let mut fields = text.split_whitespace();
            let Some(cpu_raw) = fields.next() else {
                continue;
            };
            let Some(runqueue_raw) = fields.next() else {
                continue;
            };
            let Some(timeslices_raw) = fields.next() else {
                continue;
            };
            let (Ok(task_cpu), Ok(task_runqueue), Ok(task_timeslices)) = (
                cpu_raw.parse::<i64>(),
                runqueue_raw.parse::<i64>(),
                timeslices_raw.parse::<i64>(),
            ) else {
                continue;
            };
            cpu += task_cpu;
            runqueue += task_runqueue;
            timeslices += task_timeslices;
        }
    }

    Ok(json!({
        "root_pid": root,
        "pids": live_pids,
        "captured_monotonic_ns": monotonic_ns()?,
        "cpu_runtime_ns": cpu,
        "runqueue_wait_ns": runqueue,
        "timeslices": timeslices,
        "rss_kib": rss_kib,
        "peak_rss_kib": peak_rss_kib,
        "threads": threads,
        "rchar": io_totals.get("rchar").copied().unwrap_or(0),
        "wchar": io_totals.get("wchar").copied().unwrap_or(0),
        "syscr": io_totals.get("syscr").copied().unwrap_or(0),
        "syscw": io_totals.get("syscw").copied().unwrap_or(0),
        "read_bytes": io_totals.get("read_bytes").copied().unwrap_or(0),
        "write_bytes": io_totals.get("write_bytes").copied().unwrap_or(0),
        "cancelled_write_bytes": io_totals.get("cancelled_write_bytes").copied().unwrap_or(0),
    }))
}

pub(crate) fn run(args: ProcessTreeArgs<'_>) -> Result<i32> {
    let data = snapshot(args.pid)?;
    let text = serde_json::to_string(&data)? + "\n";
    if let Some(path) = args.output {
        fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        print!("{text}");
    }
    let has_pids = data
        .get("pids")
        .and_then(Value::as_array)
        .is_some_and(|pids| !pids.is_empty());
    Ok(if has_pids { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::{descendants, parse_stat, snapshot};
    use anyhow::{Context, Result};

    #[test]
    fn current_process_stat_and_descendants_are_visible() -> Result<()> {
        let pid = std::process::id();
        assert_eq!(parse_stat(pid).map(|value| value.0), Some(pid));
        assert!(descendants(pid)?.contains(&pid));
        Ok(())
    }

    #[test]
    fn snapshot_contains_current_process_and_monotonic_time() -> Result<()> {
        let pid = std::process::id();
        let data = snapshot(pid)?;
        let pids = data["pids"].as_array().context("pids missing")?;
        assert!(
            pids.iter()
                .any(|value| value.as_u64() == Some(u64::from(pid)))
        );
        assert!(
            data["captured_monotonic_ns"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(data["threads"].as_i64().is_some_and(|value| value >= 1));
        Ok(())
    }
}
