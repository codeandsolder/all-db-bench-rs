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

DEFAULT_LOCK = Path("/run/lock/all-db-bench-performance.lock")
DEFAULT_STATUS = Path("/srv/scratch/db-bench-work/reopen-quick/idle-status.json")
DEFAULT_MANIFEST = Path("/srv/scratch/db-bench-work/reopen-quick/bin/manifest.json")
RUN_ID = "20261007-kv-reopen-warm-quick-v1"


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


def write_status(path: Path, **fields: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {"updated_at": datetime.now(timezone.utc).isoformat(), "pid": os.getpid(), **fields}
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
    tmp.replace(path)


def parse_io_full_avg10(raw: str) -> float:
    for line in raw.splitlines():
        parts = line.split()
        if parts and parts[0] == "full":
            for part in parts[1:]:
                if part.startswith("avg10="):
                    return float(part.split("=", 1)[1])
    raise ValueError("/proc/pressure/io did not contain full avg10")


def preflight_host(repo: Path, *, max_io_full_avg10: float = 5.0) -> int:
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


def load_and_verify_binary(path: Path) -> dict[str, Any]:
    data = json.loads(path.read_text())
    item = data["kv"]
    binary = Path(item["path"])
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError(f"pinned binary is not executable: {binary}")
    actual = sha256(binary)
    if actual != item["sha256"]:
        raise RuntimeError(f"pinned binary SHA-256 mismatch: expected {item['sha256']}, got {actual}")
    return data


def run_complete(repo: Path, run_id: str) -> bool:
    run_dir = repo / "results" / "runs" / run_id
    summary_path = run_dir / "summary.json"
    support_path = run_dir / "support.json"
    cases_dir = run_dir / "cases"
    if not summary_path.is_file() or not support_path.is_file() or not cases_dir.is_dir():
        return False
    try:
        summary = json.loads(summary_path.read_text())
        support = json.loads(support_path.read_text())
    except (OSError, json.JSONDecodeError):
        return False
    expected = int(support.get("case_count", -1))
    trials = int(support.get("trials", 0))
    case_count = sum(1 for case in cases_dir.glob("*.json") if case.is_file())
    failures = run_dir / "failures.ndjson"
    return (
        expected > 0
        and trials > 0
        and expected % trials == 0
        and case_count == expected
        and summary.get("row_count") == expected
        and summary.get("group_count") == expected // trials
        and not summary.get("problems")
        and (not failures.exists() or failures.stat().st_size == 0)
    )


def stage_env(repo: Path, manifest: dict[str, Any]) -> dict[str, str]:
    item = manifest["kv"]
    env = os.environ.copy()
    env.update({
        "ROOT": str(repo),
        "RUN_ID": RUN_ID,
        "MATRIX_RESUME_SHUFFLE_REMAINING": "1",
        "BENCH_BIN": item["path"],
        "BENCH_BIN_SHA256": item["sha256"],
    })
    return env


def main() -> int:
    parser = argparse.ArgumentParser(description="Run warm reopen quick campaign opportunistically")
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--binary-manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--lock-file", type=Path, default=DEFAULT_LOCK)
    parser.add_argument("--status-file", type=Path, default=DEFAULT_STATUS)
    parser.add_argument("--busy-sleep", type=float, default=1.0)
    args = parser.parse_args()
    try:
        lock_handle = acquire_lock(args.lock_file)
    except RuntimeError as error:
        print(error, flush=True)
        return 73
    with lock_handle:
        try:
            manifest = load_and_verify_binary(args.binary_manifest)
        except (OSError, KeyError, json.JSONDecodeError, RuntimeError) as error:
            write_status(args.status_file, state="failed", error=str(error))
            print(error, flush=True)
            return 2
        print(f"pinned reopen binary verified at source commit {manifest.get('repo_commit')}", flush=True)
        if run_complete(args.repo, RUN_ID):
            write_status(args.status_file, state="complete", run_id=RUN_ID)
            return 0
        busy_events = 0
        runner = args.repo / "scripts" / "run-reopen-matrix.sh"
        while True:
            preflight = preflight_host(args.repo)
            if preflight == 75:
                busy_events += 1
                write_status(args.status_file, state="waiting-for-idle", run_id=RUN_ID, busy_events=busy_events)
                if busy_events == 1 or busy_events % 30 == 0:
                    print(f"reopen queue waiting for idle: run={RUN_ID} busy_events={busy_events}", flush=True)
                time.sleep(max(args.busy_sleep, 0.1))
                continue
            if preflight != 0:
                write_status(args.status_file, state="failed", run_id=RUN_ID, returncode=preflight, error="host preflight failed")
                return preflight
            write_status(args.status_file, state="running", run_id=RUN_ID, busy_events=busy_events)
            proc = subprocess.run([str(runner), "quick", "warm"], cwd=args.repo, env=stage_env(args.repo, manifest), check=False)
            if proc.returncode == 0:
                if not run_complete(args.repo, RUN_ID):
                    write_status(args.status_file, state="failed", run_id=RUN_ID, error="runner returned success but final validation failed")
                    return 2
                write_status(args.status_file, state="complete", run_id=RUN_ID, busy_events=busy_events)
                print("warm reopen quick queue complete", flush=True)
                return 0
            if proc.returncode == 75:
                busy_events += 1
                write_status(args.status_file, state="waiting-for-idle", run_id=RUN_ID, busy_events=busy_events)
                time.sleep(max(args.busy_sleep, 0.1))
                continue
            write_status(args.status_file, state="failed", run_id=RUN_ID, returncode=proc.returncode)
            return proc.returncode


if __name__ == "__main__":
    raise SystemExit(main())
