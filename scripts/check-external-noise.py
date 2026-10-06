#!/usr/bin/env -S uv run --script
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import time
from dataclasses import asdict, dataclass
from pathlib import Path

BUILD_RE = re.compile(r"(?:^|[ /])(?:cargo(?:-[A-Za-z0-9_.-]+)?|rustc|clippy-driver|cc1plus|clang(?:\+\+)?|gcc|g\+\+|cmake|ninja|make)(?:\s|$)")
WIDE_SCAN_RE = re.compile(r"(?:^|\s)(?:find|rg|ripgrep)\s+(?:/root\b|/srv\b|/opt\b|/home\b|/mnt\b)")
ALLOWED_RE = re.compile(r"(?:run-io-contention-matrix\.sh|/kvbench(?:\s|$)|fio --name=pressure-(?:read|write))")
SCCACHE_WORKER_RE = re.compile(r"(?:^|/)sccache-dist server(?:\s|$)")


@dataclass(frozen=True)
class ProcessRow:
    pid: int
    ppid: int
    nice: int
    cpu_percent: float
    comm: str
    args: str


def classify_process(row: ProcessRow, *, ignore_nice_at_least: int = 15) -> str | None:
    if row.nice >= ignore_nice_at_least or not row.args or ALLOWED_RE.search(row.args):
        return None
    if WIDE_SCAN_RE.search(row.args) and row.cpu_percent >= 1.0:
        return "wide-filesystem-scan"
    if BUILD_RE.search(row.args) and row.cpu_percent >= 2.0:
        return "compiler-or-build"
    if row.cpu_percent >= 50.0:
        return "foreign-high-cpu"
    return None


def _proc_cpu(pid: int) -> tuple[int, int] | None:
    try:
        raw = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
        close = raw.rfind(")")
        if close < 0:
            return None
        fields = raw[close + 2 :].split()
        # fields starts at procfs field 3 (state): utime=14, stime=15, starttime=22.
        total_ticks = int(fields[11]) + int(fields[12])
        start_ticks = int(fields[19])
        return total_ticks, start_ticks
    except (FileNotFoundError, PermissionError, IndexError, ValueError, OSError):
        return None


def cpu_percent_from_samples(
    before: tuple[int, int] | None,
    after: tuple[int, int],
    *,
    elapsed_s: float,
    uptime_s: float,
    ticks_per_second: int,
) -> float:
    after_ticks, after_start = after
    if before is not None and before[1] == after_start:
        cpu_s = max(after_ticks - before[0], 0) / ticks_per_second
        return 100.0 * cpu_s / max(elapsed_s, 1e-9)
    # Process was born during the sample. Use its lifetime average over its short age.
    age_s = max(uptime_s - after_start / ticks_per_second, 1.0 / ticks_per_second)
    return 100.0 * (after_ticks / ticks_per_second) / age_s


def _metadata_rows() -> list[tuple[int, int, int, str, str]]:
    proc = subprocess.run(
        ["ps", "-eo", "pid=,ppid=,ni=,comm=,args="],
        check=True,
        capture_output=True,
        text=True,
    )
    rows: list[tuple[int, int, int, str, str]] = []
    for raw in proc.stdout.splitlines():
        parts = raw.strip().split(None, 4)
        if len(parts) < 5:
            continue
        try:
            rows.append((int(parts[0]), int(parts[1]), int(parts[2]), parts[3], parts[4]))
        except ValueError:
            continue
    return rows


def process_rows(*, sample_seconds: float = 0.25) -> list[ProcessRow]:
    first_pids = [int(p.name) for p in Path("/proc").iterdir() if p.name.isdigit()]
    before = {pid: stat for pid in first_pids if (stat := _proc_cpu(pid)) is not None}
    started = time.monotonic()
    time.sleep(max(sample_seconds, 0.0))
    elapsed = max(time.monotonic() - started, 1e-9)
    try:
        uptime_s = float(Path("/proc/uptime").read_text(encoding="utf-8").split()[0])
    except (OSError, ValueError, IndexError):
        uptime_s = 0.0
    ticks_per_second = int(os.sysconf("SC_CLK_TCK"))
    rows: list[ProcessRow] = []
    for pid, ppid, nice, comm, args in _metadata_rows():
        after = _proc_cpu(pid)
        if after is None:
            continue
        cpu_percent = cpu_percent_from_samples(
            before.get(pid),
            after,
            elapsed_s=elapsed,
            uptime_s=uptime_s,
            ticks_per_second=ticks_per_second,
        )
        rows.append(ProcessRow(pid, ppid, nice, cpu_percent, comm, args))
    return rows


def ancestor_pids(pid: int) -> set[int]:
    out: set[int] = set()
    current = pid
    while current > 1 and current not in out:
        out.add(current)
        try:
            raw = Path(f"/proc/{current}/stat").read_text(encoding="utf-8")
            close = raw.rfind(")")
            fields = raw[close + 2 :].split()
            current = int(fields[1])  # procfs field 4 (ppid), with fields starting at field 3.
        except (FileNotFoundError, PermissionError, IndexError, ValueError, OSError):
            break
    out.add(1)
    return out


def descendant_pids(rows: list[ProcessRow], roots: set[int]) -> set[int]:
    descendants = set(roots)
    changed = True
    while changed:
        changed = False
        for row in rows:
            if row.pid not in descendants and row.ppid in descendants:
                descendants.add(row.pid)
                changed = True
    return descendants


def low_priority_sccache_tree(rows: list[ProcessRow], *, min_nice: int = 10) -> set[int]:
    roots = {row.pid for row in rows if row.nice >= min_nice and SCCACHE_WORKER_RE.search(row.args)}
    return descendant_pids(rows, roots) if roots else set()


def aggregate_foreign_cpu_percent(
    rows: list[ProcessRow],
    *,
    excluded_pids: set[int],
    ignore_nice_at_least: int = 15,
) -> float:
    total = 0.0
    for row in rows:
        if row.pid in excluded_pids or row.nice >= ignore_nice_at_least or not row.args or ALLOWED_RE.search(row.args):
            continue
        total += row.cpu_percent
    return total


def main() -> int:
    parser = argparse.ArgumentParser(description="Reject normal-priority foreign CPU/build work during performance benchmarks")
    parser.add_argument("--ignore-nice-at-least", type=int, default=15)
    parser.add_argument("--json-out")
    parser.add_argument("--exclude-pid", type=int, action="append", default=[])
    parser.add_argument("--max-aggregate-cpu-percent", type=float, default=50.0)
    parser.add_argument("--sample-ms", type=float, default=250.0)
    args = parser.parse_args()

    ancestors = ancestor_pids(os.getpid())
    rows = process_rows(sample_seconds=max(args.sample_ms, 0.0) / 1000.0)
    own_tree = descendant_pids(rows, {os.getpid()})
    sccache_worker_tree = low_priority_sccache_tree(rows)
    explicit_excluded_tree = descendant_pids(rows, set(args.exclude_pid)) if args.exclude_pid else set()
    excluded = ancestors | own_tree | sccache_worker_tree | explicit_excluded_tree
    offenders = []
    for row in rows:
        if row.pid in excluded:
            continue
        reason = classify_process(row, ignore_nice_at_least=args.ignore_nice_at_least)
        if reason is not None:
            offenders.append({**asdict(row), "reason": reason})
    offenders.sort(key=lambda item: (-item["cpu_percent"], item["pid"]))
    aggregate_cpu_percent = aggregate_foreign_cpu_percent(
        rows, excluded_pids=excluded, ignore_nice_at_least=args.ignore_nice_at_least
    )
    aggregate_cpu_exceeded = aggregate_cpu_percent >= args.max_aggregate_cpu_percent
    quiet = not offenders and not aggregate_cpu_exceeded
    result = {
        "quiet": quiet,
        "cpu_sample_ms": args.sample_ms,
        "ignore_nice_at_least": args.ignore_nice_at_least,
        "max_aggregate_cpu_percent": args.max_aggregate_cpu_percent,
        "aggregate_foreign_cpu_percent": aggregate_cpu_percent,
        "aggregate_cpu_exceeded": aggregate_cpu_exceeded,
        "excluded_pids": sorted(explicit_excluded_tree),
        "offenders": offenders,
    }
    encoded = json.dumps(result, sort_keys=True, separators=(",", ":"))
    if args.json_out:
        with open(args.json_out, "w", encoding="utf-8") as handle:
            handle.write(encoded + "\n")
    else:
        print(encoded)
    return 0 if quiet else 75


if __name__ == "__main__":
    raise SystemExit(main())
