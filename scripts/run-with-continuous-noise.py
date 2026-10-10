#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import time
from collections import defaultdict
from pathlib import Path
from typing import Any

HZ = int(os.sysconf("SC_CLK_TCK"))


def proc_stat(pid: int) -> tuple[int, int, int, int] | None:
    """Return (ppid, nice, cpu_ticks, start_ticks)."""
    try:
        raw = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
        close = raw.rfind(")")
        if close < 0:
            return None
        f = raw[close + 2 :].split()
        return int(f[1]), int(f[16]), int(f[11]) + int(f[12]), int(f[19])
    except (FileNotFoundError, PermissionError, IndexError, ValueError, OSError):
        return None


def proc_io(pid: int) -> tuple[int, int] | None:
    try:
        values: dict[str, int] = {}
        for line in Path(f"/proc/{pid}/io").read_text(encoding="utf-8").splitlines():
            key, value = line.split(":", 1)
            if key in {"read_bytes", "write_bytes"}:
                values[key] = int(value.strip())
        return values.get("read_bytes", 0), values.get("write_bytes", 0)
    except (FileNotFoundError, PermissionError, ValueError, OSError):
        return None


def proc_cmd(pid: int) -> str:
    try:
        raw = Path(f"/proc/{pid}/cmdline").read_bytes()
        text = raw.replace(b"\0", b" ").decode("utf-8", errors="replace").strip()
        if text:
            return text
        return Path(f"/proc/{pid}/comm").read_text(encoding="utf-8").strip()
    except (FileNotFoundError, PermissionError, OSError):
        return ""


def pids() -> list[int]:
    out = []
    try:
        entries = os.scandir("/proc")
    except OSError:
        return out
    with entries:
        for entry in entries:
            if entry.name.isdigit():
                out.append(int(entry.name))
    return out


def ancestor_pids(pid: int) -> set[int]:
    out: set[int] = set()
    current = pid
    while current > 1 and current not in out:
        out.add(current)
        stat = proc_stat(current)
        if stat is None:
            break
        current = stat[0]
    out.add(1)
    return out


def descendants(meta: dict[int, tuple[int, int, int, int]], roots: set[int]) -> set[int]:
    out = set(roots)
    changed = True
    while changed:
        changed = False
        for pid, (ppid, _nice, _cpu, _start) in meta.items():
            if pid not in out and ppid in out:
                out.add(pid)
                changed = True
    return out


def sample_proc() -> tuple[
    dict[int, tuple[int, int, int, int]],
    dict[tuple[int, int], tuple[int, int]],
]:
    meta: dict[int, tuple[int, int, int, int]] = {}
    io: dict[tuple[int, int], tuple[int, int]] = {}
    for pid in pids():
        st = proc_stat(pid)
        if st is None:
            continue
        meta[pid] = st
        counters = proc_io(pid)
        if counters is not None:
            io[(pid, st[3])] = counters
    return meta, io


def psi_io_full_total_us() -> int:
    try:
        for line in Path("/proc/pressure/io").read_text(encoding="utf-8").splitlines():
            if line.startswith("full "):
                for part in line.split():
                    if part.startswith("total="):
                        return int(part.split("=", 1)[1])
    except (OSError, ValueError):
        pass
    return 0


def contamination_reasons(
    *,
    total_io_bytes: int,
    peak_io_rate_bytes_s: float,
    peak_cpu_percent: float,
    max_io_bytes: int,
    max_io_rate_mib_s: float,
    max_cpu_percent: float,
) -> list[str]:
    reasons: list[str] = []
    if total_io_bytes > max_io_bytes:
        reasons.append("foreign-io-total")
    if peak_io_rate_bytes_s > max_io_rate_mib_s * 1024 * 1024:
        reasons.append("foreign-io-rate")
    if peak_cpu_percent >= max_cpu_percent:
        reasons.append("foreign-cpu")
    return reasons


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run a benchmark child while continuously tracking attributable foreign CPU and I/O."
    )
    parser.add_argument("--json-out", required=True)
    parser.add_argument("--sample-ms", type=float, default=250.0)
    parser.add_argument("--ignore-cpu-nice-at-least", type=int, default=15)
    parser.add_argument("--max-foreign-cpu-percent", type=float, default=50.0)
    parser.add_argument("--max-foreign-io-bytes", type=int, default=8 * 1024 * 1024)
    parser.add_argument("--max-foreign-io-rate-mib-s", type=float, default=8.0)
    parser.add_argument("--observe-only", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = list(args.command)
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        parser.error("command is required after --")
    if args.sample_ms <= 0:
        parser.error("--sample-ms must be positive")
    if args.max_foreign_cpu_percent < 0 or args.max_foreign_io_bytes < 0 or args.max_foreign_io_rate_mib_s < 0:
        parser.error("thresholds must be non-negative")

    start_uptime_s = float(Path("/proc/uptime").read_text(encoding="utf-8").split()[0])
    start_ticks = int(start_uptime_s * HZ)
    own_ancestors = ancestor_pids(os.getpid())
    meta_prev, io_prev = sample_proc()
    cpu_prev = {(pid, st[3]): st[2] for pid, st in meta_prev.items()}
    psi_start = psi_io_full_total_us()

    started = time.monotonic()
    child = subprocess.Popen(command, start_new_session=True)
    child_root = child.pid
    monitor_nice_before = os.getpriority(os.PRIO_PROCESS, 0)
    if monitor_nice_before < 15:
        os.nice(15 - monitor_nice_before)
    monitor_nice = os.getpriority(os.PRIO_PROCESS, 0)

    total_read = 0
    total_write = 0
    max_io_rate = 0.0
    max_cpu_percent = 0.0
    samples = 0
    attributed: dict[tuple[int, int], dict[str, Any]] = defaultdict(
        lambda: {"read_bytes": 0, "write_bytes": 0, "cpu_ticks": 0, "max_io_rate_bps": 0.0, "command": ""}
    )
    child_identities: set[tuple[int, int]] = set()
    last_sample = started
    child_rc: int | None = None

    try:
        while True:
            rc = child.poll()
            now = time.monotonic()
            if rc is None and now - last_sample < args.sample_ms / 1000.0:
                time.sleep(min(args.sample_ms / 1000.0 - (now - last_sample), 0.05))
                continue

            meta, io_now = sample_proc()
            elapsed = max(time.monotonic() - last_sample, 1e-9)
            child_tree = descendants(meta, {child_root})
            for pid in child_tree:
                st = meta.get(pid)
                if st is not None:
                    child_identities.add((pid, st[3]))
            excluded = own_ancestors | child_tree | {os.getpid()}
            interval_read = 0
            interval_write = 0
            interval_cpu_ticks = 0

            for pid, st in meta.items():
                ppid, nice, cpu_ticks, proc_start = st
                key = (pid, proc_start)
                if pid in excluded or key in child_identities or pid == 2 or ppid == 2:
                    continue

                current_io = io_now.get(key)
                previous_io = io_prev.get(key)
                if current_io is not None:
                    if previous_io is not None:
                        dr = max(current_io[0] - previous_io[0], 0)
                        dw = max(current_io[1] - previous_io[1], 0)
                    elif proc_start >= start_ticks:
                        dr, dw = current_io
                    else:
                        dr = dw = 0
                    if dr or dw:
                        interval_read += dr
                        interval_write += dw
                        item = attributed[key]
                        if not item["command"]:
                            item["command"] = proc_cmd(pid)
                        item["read_bytes"] += dr
                        item["write_bytes"] += dw
                        rate = (dr + dw) / elapsed
                        item["max_io_rate_bps"] = max(item["max_io_rate_bps"], rate)

                previous_cpu = cpu_prev.get(key)
                if previous_cpu is not None:
                    dcpu = max(cpu_ticks - previous_cpu, 0)
                elif proc_start >= start_ticks:
                    dcpu = cpu_ticks
                else:
                    dcpu = 0
                if nice < args.ignore_cpu_nice_at_least and dcpu:
                    interval_cpu_ticks += dcpu
                    item = attributed[key]
                    if not item["command"]:
                        item["command"] = proc_cmd(pid)
                    item["cpu_ticks"] += dcpu

            total_read += interval_read
            total_write += interval_write
            interval_io_rate = (interval_read + interval_write) / elapsed
            interval_cpu_percent = 100.0 * (interval_cpu_ticks / HZ) / elapsed
            max_io_rate = max(max_io_rate, interval_io_rate)
            max_cpu_percent = max(max_cpu_percent, interval_cpu_percent)
            samples += 1

            io_prev = io_now
            cpu_prev = {(pid, st[3]): st[2] for pid, st in meta.items()}
            last_sample = time.monotonic()

            if rc is not None:
                child_rc = rc
                break
    except BaseException:
        if child.poll() is None:
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                child.wait()
        raise

    duration = max(time.monotonic() - started, 1e-9)
    psi_delta_us = max(psi_io_full_total_us() - psi_start, 0)
    top = []
    for (pid, proc_start), item in attributed.items():
        io_bytes = int(item["read_bytes"]) + int(item["write_bytes"])
        cpu_s = float(item["cpu_ticks"]) / HZ
        if io_bytes == 0 and cpu_s == 0:
            continue
        top.append(
            {
                "pid": pid,
                "start_ticks": proc_start,
                "command": str(item["command"]) or proc_cmd(pid),
                "read_bytes": int(item["read_bytes"]),
                "write_bytes": int(item["write_bytes"]),
                "io_bytes": io_bytes,
                "cpu_seconds": cpu_s,
                "max_io_rate_mib_s": float(item["max_io_rate_bps"]) / (1024 * 1024),
            }
        )
    top.sort(key=lambda x: (x["io_bytes"], x["cpu_seconds"]), reverse=True)
    top = top[:20]

    total_io = total_read + total_write
    contaminated_reasons = contamination_reasons(
        total_io_bytes=total_io,
        peak_io_rate_bytes_s=max_io_rate,
        peak_cpu_percent=max_cpu_percent,
        max_io_bytes=args.max_foreign_io_bytes,
        max_io_rate_mib_s=args.max_foreign_io_rate_mib_s,
        max_cpu_percent=args.max_foreign_cpu_percent,
    )

    result = {
        "monitor_version": 1,
        "command": command,
        "child_pid": child_root,
        "child_returncode": child_rc,
        "duration_s": duration,
        "sample_ms": args.sample_ms,
        "sample_count": samples,
        "monitor_nice_before": monitor_nice_before,
        "monitor_nice": monitor_nice,
        "ignore_cpu_nice_at_least": args.ignore_cpu_nice_at_least,
        "max_foreign_cpu_percent": args.max_foreign_cpu_percent,
        "max_foreign_io_bytes": args.max_foreign_io_bytes,
        "max_foreign_io_rate_mib_s": args.max_foreign_io_rate_mib_s,
        "foreign_read_bytes": total_read,
        "foreign_write_bytes": total_write,
        "foreign_io_bytes": total_io,
        "peak_foreign_io_rate_mib_s": max_io_rate / (1024 * 1024),
        "peak_foreign_cpu_percent": max_cpu_percent,
        "system_io_full_psi_fraction": psi_delta_us / 1e6 / duration,
        "contaminated": bool(contaminated_reasons),
        "contamination_reasons": contaminated_reasons,
        "top_foreign_processes": top,
        "observe_only": args.observe_only,
    }
    out = Path(args.json_out)
    out.parent.mkdir(parents=True, exist_ok=True)
    tmp = out.with_suffix(out.suffix + ".tmp")
    tmp.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(tmp, out)

    if child_rc not in (0, None):
        return int(child_rc)
    if result["contaminated"] and not args.observe_only:
        return 75
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
