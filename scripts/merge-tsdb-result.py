#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
from pathlib import Path

COUNTERS = (
    "cpu_runtime_ns",
    "runqueue_wait_ns",
    "timeslices",
    "rchar",
    "wchar",
    "syscr",
    "syscw",
    "read_bytes",
    "write_bytes",
    "cancelled_write_bytes",
)


def load(path: Path) -> dict[str, object]:
    return json.loads(path.read_text())


def directory_bytes(path: Path) -> int:
    total = 0
    if not path.exists():
        return 0
    for item in path.rglob("*"):
        try:
            if item.is_file() and not item.is_symlink():
                total += item.stat().st_size
        except OSError:
            continue
    return total


def delta(after: int, before: int) -> int:
    return max(0, after - before)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--client", type=Path, required=True)
    parser.add_argument("--server-before", type=Path, required=True)
    parser.add_argument("--server-after", type=Path, required=True)
    parser.add_argument("--data-dir", type=Path, required=True)
    parser.add_argument("--startup-s", type=float, required=True)
    parser.add_argument("--server-log", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    result = load(args.client)
    before = load(args.server_before)
    after = load(args.server_after)
    wall_ns = delta(int(after["captured_monotonic_ns"]), int(before["captured_monotonic_ns"]))
    server = {
        key: delta(int(after.get(key, 0)), int(before.get(key, 0))) for key in COUNTERS
    }
    server.update(
        {
            "accounting_wall_ns": wall_ns,
            "cpu_runtime_fraction_of_wall": server["cpu_runtime_ns"] / wall_ns if wall_ns else 0.0,
            "runqueue_wait_fraction_of_wall": server["runqueue_wait_ns"] / wall_ns if wall_ns else 0.0,
            "rss_before_kib": int(before.get("rss_kib", 0)),
            "rss_after_kib": int(after.get("rss_kib", 0)),
            "peak_rss_after_kib": int(after.get("peak_rss_kib", 0)),
            "threads_before": int(before.get("threads", 0)),
            "threads_after": int(after.get("threads", 0)),
            "pids_before": before.get("pids", []),
            "pids_after": after.get("pids", []),
        }
    )
    result["server_startup_s"] = args.startup_s
    result["server_process"] = server
    result["server_data_bytes"] = directory_bytes(args.data_dir)
    if args.server_log:
        try:
            result["server_log_bytes"] = args.server_log.stat().st_size
        except OSError:
            result["server_log_bytes"] = 0
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
