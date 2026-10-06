#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
import os
import time
from pathlib import Path


def parse_stat(pid: int) -> tuple[int, int] | None:
    try:
        text = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    try:
        tail = text.rsplit(") ", 1)[1].split()
        ppid = int(tail[1])
        return pid, ppid
    except (IndexError, ValueError):
        return None


def descendants(root: int) -> list[int]:
    parents: dict[int, int] = {}
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        item = parse_stat(int(name))
        if item is not None:
            pid, ppid = item
            parents[pid] = ppid
    out = {root}
    changed = True
    while changed:
        changed = False
        for pid, ppid in parents.items():
            if ppid in out and pid not in out:
                out.add(pid)
                changed = True
    return sorted(pid for pid in out if Path(f"/proc/{pid}").exists())


def kv_file(path: Path) -> dict[str, int]:
    out: dict[str, int] = {}
    try:
        text = path.read_text()
    except OSError:
        return out
    for line in text.splitlines():
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        token = value.strip().split()[0] if value.strip() else ""
        try:
            out[key] = int(token)
        except ValueError:
            pass
    return out


def snapshot(root: int) -> dict[str, object]:
    pids = descendants(root)
    cpu = runqueue = timeslices = 0
    rss_kib = peak_rss_kib = threads = 0
    io_totals = {key: 0 for key in ("rchar", "wchar", "syscr", "syscw", "read_bytes", "write_bytes", "cancelled_write_bytes")}
    live_pids: list[int] = []
    for pid in pids:
        base = Path(f"/proc/{pid}")
        if not base.exists():
            continue
        live_pids.append(pid)
        status = kv_file(base / "status")
        rss_kib += status.get("VmRSS", 0)
        peak_rss_kib += status.get("VmHWM", 0)
        threads += status.get("Threads", 0)
        proc_io = kv_file(base / "io")
        for key in io_totals:
            io_totals[key] += proc_io.get(key, 0)
        try:
            task_dirs = list((base / "task").iterdir())
        except OSError:
            task_dirs = []
        for task in task_dirs:
            try:
                fields = (task / "schedstat").read_text().split()
                cpu += int(fields[0])
                runqueue += int(fields[1])
                timeslices += int(fields[2])
            except (OSError, IndexError, ValueError):
                continue
    return {
        "root_pid": root,
        "pids": live_pids,
        "captured_monotonic_ns": time.monotonic_ns(),
        "cpu_runtime_ns": cpu,
        "runqueue_wait_ns": runqueue,
        "timeslices": timeslices,
        "rss_kib": rss_kib,
        "peak_rss_kib": peak_rss_kib,
        "threads": threads,
        **io_totals,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--pid", type=int, required=True)
    parser.add_argument("--output")
    args = parser.parse_args()
    data = snapshot(args.pid)
    text = json.dumps(data, sort_keys=True)
    if args.output:
        Path(args.output).write_text(text + "\n")
    else:
        print(text)
    return 0 if data["pids"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
