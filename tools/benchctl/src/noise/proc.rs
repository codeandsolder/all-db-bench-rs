use anyhow::{Context, Result};
use nix::unistd::{SysconfVar, sysconf};
use std::{collections::BTreeSet, fs};

pub(super) fn proc_pids() -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .collect()
}

pub(super) fn clock_ticks_per_second() -> Result<u64> {
    let ticks = sysconf(SysconfVar::CLK_TCK)
        .context("sysconf CLK_TCK failed")?
        .context("CLK_TCK unavailable")?;
    u64::try_from(ticks).context("CLK_TCK was non-positive")
}

pub(super) fn uptime_seconds() -> f64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0)
}

pub(super) fn proc_ppid(pid: u32) -> Option<u32> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = raw.rfind(')')?;
    raw.get(close + 2..)?
        .split_whitespace()
        .nth(1)?
        .parse::<u32>()
        .ok()
}

pub(super) fn ancestor_pids(pid: u32) -> BTreeSet<u32> {
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
