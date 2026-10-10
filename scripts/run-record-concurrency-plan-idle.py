#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, TextIO

from idle_supervisor_common import storage_preflight, write_status as supervisor_write_status


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def acquire_lock(path: Path) -> TextIO:
    path.parent.mkdir(parents=True, exist_ok=True)
    handle = path.open("a+", encoding="utf-8")
    try:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        handle.close()
        raise RuntimeError(f"benchmark campaign lock is already held: {path}") from None
    handle.seek(0)
    handle.truncate()
    handle.write(f"pid={os.getpid()}\n")
    handle.flush()
    return handle


def write_status(path: Path, **fields: Any) -> bool:
    return supervisor_write_status(path, **fields)


def parse_io_full_avg10(raw: str) -> float:
    for line in raw.splitlines():
        parts = line.split()
        if parts and parts[0] == "full":
            for part in parts[1:]:
                if part.startswith("avg10="):
                    return float(part.split("=", 1)[1])
    raise ValueError("/proc/pressure/io did not contain full avg10")


def preflight_host(repo: Path, *, max_io_full_avg10: float = 5.0) -> int:
    storage_rc = storage_preflight(repo)
    if storage_rc != 0:
        return storage_rc
    try:
        pressure = parse_io_full_avg10(Path("/proc/pressure/io").read_text())
    except (OSError, ValueError):
        return 2
    if pressure > max_io_full_avg10:
        return 75
    proc = subprocess.run(
        [sys.executable, str(repo / "scripts" / "check-external-noise.py"), "--sample-ms", "250"],
        cwd=repo,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return proc.returncode


def verify_binary(path: Path, expected_sha256: str) -> None:
    if not path.is_file() or not os.access(path, os.X_OK):
        raise RuntimeError(f"pinned binary is not executable: {path}")
    actual = sha256(path)
    if actual != expected_sha256:
        raise RuntimeError(f"pinned binary SHA-256 mismatch: expected {expected_sha256}, got {actual}")


def run_complete(repo: Path, run_id: str) -> bool:
    run = repo / "results" / "runs" / run_id
    try:
        support = json.loads((run / "support.json").read_text())
        summary = json.loads((run / "summary.json").read_text())
    except (OSError, json.JSONDecodeError):
        return False
    failures = run / "failures.ndjson"
    expected = int(support.get("case_count", -1))
    trials = int(support.get("expect_trials", support.get("trials", 0)))
    expected_groups = expected // trials if trials > 0 and expected % trials == 0 else -1
    return (
        expected > 0
        and trials > 0
        and expected_groups > 0
        and summary.get("row_count") == expected
        and summary.get("group_count") == expected_groups
        and not summary.get("problems")
        and (not failures.exists() or failures.stat().st_size == 0)
    )


def main() -> int:
    parser = argparse.ArgumentParser(description="Opportunistically run an exact record concurrency plan")
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--bench-bin", type=Path, required=True)
    parser.add_argument("--bench-bin-sha256", required=True)
    parser.add_argument("--rocks-bench-bin", type=Path, required=True)
    parser.add_argument("--rocks-bench-bin-sha256", required=True)
    parser.add_argument("--lock-file", type=Path, default=Path("/run/lock/all-db-bench-performance.lock"))
    parser.add_argument("--status-file", type=Path, required=True)
    parser.add_argument("--busy-sleep", type=float, default=1.0)
    args = parser.parse_args()

    try:
        lock_handle = acquire_lock(args.lock_file)
    except RuntimeError as error:
        print(error, flush=True)
        return 73

    with lock_handle:
        try:
            verify_binary(args.bench_bin, args.bench_bin_sha256)
            verify_binary(args.rocks_bench_bin, args.rocks_bench_bin_sha256)
        except RuntimeError as error:
            write_status(args.status_file, state="failed", error=str(error))
            print(error, flush=True)
            return 2

        if run_complete(args.repo, args.run_id):
            write_status(args.status_file, state="complete", run_id=args.run_id, busy_events=0)
            print(f"skip complete record concurrency plan: {args.run_id}", flush=True)
            return 0

        busy_events = 0
        runner = args.repo / "scripts" / "run-record-concurrency-plan.sh"
        env = os.environ.copy()
        env.update(
            {
                "ROOT": str(args.repo),
                "BENCH_BIN": str(args.bench_bin),
                "BENCH_BIN_SHA256": args.bench_bin_sha256,
                "ROCKS_BENCH_BIN": str(args.rocks_bench_bin),
                "ROCKS_BENCH_BIN_SHA256": args.rocks_bench_bin_sha256,
                "MATRIX_RESUME_SHUFFLE_REMAINING": "1",
            }
        )

        while True:
            preflight = preflight_host(args.repo)
            if preflight == 75:
                busy_events += 1
                write_status(
                    args.status_file,
                    state="waiting-for-idle",
                    run_id=args.run_id,
                    busy_events=busy_events,
                )
                if busy_events == 1 or busy_events % 30 == 0:
                    print(
                        f"record concurrency plan waiting for idle: "
                        f"run={args.run_id} busy_events={busy_events}",
                        flush=True,
                    )
                time.sleep(max(args.busy_sleep, 0.1))
                continue
            if preflight != 0:
                write_status(
                    args.status_file,
                    state="failed",
                    run_id=args.run_id,
                    returncode=preflight,
                    error="host preflight failed",
                )
                return preflight

            write_status(
                args.status_file,
                state="running",
                run_id=args.run_id,
                busy_events=busy_events,
            )
            proc = subprocess.run(
                [str(runner), str(args.plan), args.run_id],
                cwd=args.repo,
                env=env,
                check=False,
            )
            if proc.returncode == 0:
                if not run_complete(args.repo, args.run_id):
                    write_status(
                        args.status_file,
                        state="failed",
                        run_id=args.run_id,
                        error="runner returned success but final validation failed",
                    )
                    return 2
                write_status(
                    args.status_file,
                    state="complete",
                    run_id=args.run_id,
                    busy_events=busy_events,
                )
                print(f"record concurrency plan complete: {args.run_id}", flush=True)
                return 0
            if proc.returncode == 75:
                busy_events += 1
                write_status(
                    args.status_file,
                    state="waiting-for-idle",
                    run_id=args.run_id,
                    busy_events=busy_events,
                )
                time.sleep(max(args.busy_sleep, 0.1))
                continue

            write_status(
                args.status_file,
                state="failed",
                run_id=args.run_id,
                returncode=proc.returncode,
            )
            return proc.returncode


if __name__ == "__main__":
    raise SystemExit(main())
