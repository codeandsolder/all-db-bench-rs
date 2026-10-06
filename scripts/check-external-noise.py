#!/usr/bin/env -S uv run --script
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
from dataclasses import asdict, dataclass

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


def ancestor_pids(pid: int) -> set[int]:
    out: set[int] = set()
    current = pid
    while current > 1 and current not in out:
        out.add(current)
        try:
            fields = open(f"/proc/{current}/stat", encoding="utf-8").read().split()
            current = int(fields[3])
        except (FileNotFoundError, IndexError, ValueError, OSError):
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

def process_rows() -> list[ProcessRow]:
    proc = subprocess.run(
        ["ps", "-eo", "pid=,ppid=,ni=,pcpu=,comm=,args="],
        check=True,
        capture_output=True,
        text=True,
    )
    rows: list[ProcessRow] = []
    for raw in proc.stdout.splitlines():
        parts = raw.strip().split(None, 5)
        if len(parts) < 6:
            continue
        try:
            rows.append(ProcessRow(int(parts[0]), int(parts[1]), int(parts[2]), float(parts[3]), parts[4], parts[5]))
        except ValueError:
            continue
    return rows


def main() -> int:
    parser = argparse.ArgumentParser(description="Reject normal-priority foreign CPU/build work during performance benchmarks")
    parser.add_argument("--ignore-nice-at-least", type=int, default=15)
    parser.add_argument("--json-out")
    parser.add_argument("--exclude-pid", type=int, action="append", default=[])
    parser.add_argument("--max-aggregate-cpu-percent", type=float, default=50.0)
    args = parser.parse_args()

    ancestors = ancestor_pids(os.getpid())
    rows = process_rows()
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
    result = {
        "quiet": not offenders and not aggregate_cpu_exceeded,
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
    return 0 if not offenders else 75


if __name__ == "__main__":
    raise SystemExit(main())
