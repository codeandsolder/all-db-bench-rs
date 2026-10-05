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
    args = parser.parse_args()

    ancestors = ancestor_pids(os.getpid())
    offenders = []
    for row in process_rows():
        if row.pid in ancestors:
            continue
        reason = classify_process(row, ignore_nice_at_least=args.ignore_nice_at_least)
        if reason is not None:
            offenders.append({**asdict(row), "reason": reason})
    offenders.sort(key=lambda item: (-item["cpu_percent"], item["pid"]))
    result = {
        "quiet": not offenders,
        "ignore_nice_at_least": args.ignore_nice_at_least,
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
