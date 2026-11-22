use anyhow::{Context, Result, bail};
use nix::unistd::{SysconfVar, sysconf};
use std::{collections::BTreeSet, fs};

pub(super) fn proc_pids() -> Result<Vec<u32>> {
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc").context("failed to enumerate /proc")? {
        if let Some(pid) = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        {
            pids.push(pid);
        }
    }
    if pids.is_empty() {
        bail!("no visible process IDs");
    }
    Ok(pids)
}

pub(super) fn clock_ticks_per_second() -> Result<u64> {
    let ticks = sysconf(SysconfVar::CLK_TCK)
        .context("sysconf CLK_TCK failed")?
        .context("CLK_TCK unavailable")?;
    u64::try_from(ticks)
        .ok()
        .filter(|&ticks| ticks > 0)
        .context("CLK_TCK was non-positive")
}

pub(super) fn uptime_seconds() -> Result<f64> {
    let raw = fs::read_to_string("/proc/uptime").context("failed to read uptime")?;
    let seconds = raw
        .split_whitespace()
        .next()
        .context("empty /proc/uptime")?
        .parse::<f64>()?;
    if !seconds.is_finite() || seconds < 0.0 {
        bail!("invalid /proc/uptime");
    }
    Ok(seconds)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProcStat {
    pub(super) ppid: u32,
    pub(super) pgrp: u32,
    pub(super) nice: i32,
    pub(super) cpu_ticks: u64,
    pub(super) start_ticks: u64,
}

fn parse_stat(raw: &str) -> Option<ProcStat> {
    // comm can contain spaces, parentheses, and newlines.
    let close = raw.rfind(')')?;
    let mut fields = raw.get(close + 2..)?.split_whitespace();
    let ppid = fields.nth(1)?.parse().ok()?;
    let pgrp = fields.next()?.parse().ok()?;
    let user = fields.nth(8)?.parse::<u64>().ok()?;
    let system = fields.next()?.parse::<u64>().ok()?;
    let nice = fields.nth(3)?.parse().ok()?;
    let start_ticks = fields.nth(2)?.parse().ok()?;
    Some(ProcStat {
        ppid,
        pgrp,
        nice,
        cpu_ticks: user.saturating_add(system),
        start_ticks,
    })
}

pub(super) fn proc_stat(pid: u32) -> Option<ProcStat> {
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

pub(super) fn proc_io(pid: u32) -> Option<(u64, u64)> {
    parse_io(&fs::read_to_string(format!("/proc/{pid}/io")).ok()?)
}

fn parse_io(text: &str) -> Option<(u64, u64)> {
    let mut read = None;
    let mut write = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key {
            "read_bytes" => read = Some(value.trim().parse().ok()?),
            "write_bytes" => write = Some(value.trim().parse().ok()?),
            _ => {}
        }
    }
    Some((read?, write?))
}

pub(super) fn ancestor_pids(pid: u32) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    let mut current = pid;
    while current > 1 && !out.contains(&current) {
        out.insert(current);
        let Some(parent) = proc_stat(current).map(|stat| stat.ppid) else {
            break;
        };
        current = parent;
    }
    out.insert(1);
    out
}

#[cfg(test)]
mod tests {
    use super::{parse_io, parse_stat};
    use anyhow::{Context, Result};

    #[test]
    fn stat_handles_embedded_parentheses_and_negative_nice() -> Result<()> {
        let stat = parse_stat("9 (complex ) name) R 4 9 9 0 0 0 0 0 0 0 17 23 0 0 20 -5 1 0 12345")
            .context("valid stat")?;
        assert_eq!(
            (
                stat.ppid,
                stat.pgrp,
                stat.nice,
                stat.cpu_ticks,
                stat.start_ticks
            ),
            (4, 9, -5, 40, 12345)
        );
        Ok(())
    }

    #[test]
    fn io_ignores_signed_cancelled_writes_but_requires_both_counters() {
        assert_eq!(
            parse_io("rchar: 4\nread_bytes: 512\nwrite_bytes: 1024\ncancelled_write_bytes: -4096"),
            Some((512, 1024))
        );
        assert_eq!(parse_io("read_bytes: 512"), None);
        assert_eq!(parse_io("read_bytes: -1\nwrite_bytes: 0"), None);
    }
}
