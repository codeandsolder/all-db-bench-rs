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

DEFAULT_REPO = Path("/srv/scratch/db-bench-work/kv-refresh/runtime")
DEFAULT_LOCK = Path("/run/lock/all-db-bench-performance.lock")
DEFAULT_STATUS = Path("/srv/scratch/db-bench-work/kv-refresh/stock-idle-status.json")
DEFAULT_RUN_ID = "kv-quick-stock"


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
        if not parts or parts[0] != "full":
            continue
        for part in parts[1:]:
            if part.startswith("avg10="):
                return float(part.split("=", 1)[1])
    raise ValueError("/proc/pressure/io did not contain full avg10")


def preflight_host(repo: Path, *, max_io_full_avg10: float) -> int:
    try:
        io_full_avg10 = parse_io_full_avg10(Path("/proc/pressure/io").read_text())
    except (OSError, ValueError):
        return 2
    if io_full_avg10 > max_io_full_avg10:
        return 75
    proc = subprocess.run(
        [sys.executable, str(repo / "scripts" / "check-external-noise.py"), "--sample-ms", "250"],
        cwd=repo,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return proc.returncode


def complete(repo: Path, run_id: str) -> bool:
    run_dir = repo / "results" / "runs" / run_id
    summary_path = run_dir / "summary.json"
    if not summary_path.is_file():
        return False
    try:
        summary = json.loads(summary_path.read_text())
    except (OSError, json.JSONDecodeError):
        return False
    failures = run_dir / "failures.ndjson"
    return (
        summary.get("row_count") == 540
        and summary.get("group_count") == 180
        and not summary.get("problems")
        and (not failures.exists() or failures.stat().st_size == 0)
    )


def audit(repo: Path, run_id: str, audit_dir: Path, json_out: Path | None = None, markdown_out: Path | None = None) -> None:
    run_dir = repo / "results" / "runs" / run_id
    audit_dir.mkdir(parents=True, exist_ok=True)
    json_out = json_out or audit_dir / f"{run_id}-sizing.json"
    markdown_out = markdown_out or audit_dir / f"{run_id}-sizing.md"
    json_out.parent.mkdir(parents=True, exist_ok=True)
    markdown_out.parent.mkdir(parents=True, exist_ok=True)
    proc = subprocess.run(
        [
            str(repo / "scripts" / "audit_baseline_sizing.py"),
            str(run_dir / "results.ndjson"),
            "--json-out",
            str(json_out),
            "--markdown-out",
            str(markdown_out),
        ],
        cwd=repo,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"KV sizing audit failed rc={proc.returncode}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Run raw-KV quick baseline opportunistically when the host is quiet")
    parser.add_argument("--repo", type=Path, default=DEFAULT_REPO)
    parser.add_argument("--bench-bin", type=Path, required=True)
    parser.add_argument("--expected-bench-sha256")
    parser.add_argument("--run-id", default=DEFAULT_RUN_ID)
    parser.add_argument("--lock-file", type=Path, default=DEFAULT_LOCK)
    parser.add_argument("--status-file", type=Path, default=DEFAULT_STATUS)
    parser.add_argument("--audit-dir", type=Path, default=Path("/srv/scratch/db-bench-work/kv-refresh"))
    parser.add_argument("--audit-json", type=Path)
    parser.add_argument("--audit-markdown", type=Path)
    parser.add_argument("--watch", action="store_true")
    parser.add_argument("--busy-sleep", type=float, default=2.0)
    parser.add_argument("--max-io-full-avg10", type=float, default=5.0)
    args = parser.parse_args()

    if not args.bench_bin.is_file() or not os.access(args.bench_bin, os.X_OK):
        parser.error(f"benchmark binary is not executable: {args.bench_bin}")

    try:
        lock_handle = acquire_lock(args.lock_file)
    except RuntimeError as error:
        print(error, flush=True)
        return 73

    with lock_handle:
        if complete(args.repo, args.run_id):
            audit(args.repo, args.run_id, args.audit_dir, args.audit_json, args.audit_markdown)
            write_status(args.status_file, state="complete", campaign="kv-quick-stock", run_id=args.run_id, busy_events=0)
            print(f"KV quick already complete: {args.run_id}", flush=True)
            return 0

        busy_events = 0
        binary_verified = not bool(args.expected_bench_sha256)
        while True:
            preflight_rc = preflight_host(args.repo, max_io_full_avg10=args.max_io_full_avg10)
            if preflight_rc == 75:
                busy_events += 1
                write_status(
                    args.status_file,
                    state="waiting-for-idle",
                    campaign="kv-quick-stock",
                    run_id=args.run_id,
                    busy_events=busy_events,
                )
                if not args.watch:
                    return 75
                if busy_events == 1 or busy_events % 30 == 0:
                    print(f"KV quick waiting for idle: busy_events={busy_events}", flush=True)
                time.sleep(max(args.busy_sleep, 0.1))
                continue
            if preflight_rc != 0:
                write_status(args.status_file, state="failed", campaign="kv-quick-stock", run_id=args.run_id, returncode=preflight_rc)
                return preflight_rc

            if not binary_verified:
                if args.expected_bench_sha256 and sha256(args.bench_bin) != args.expected_bench_sha256:
                    write_status(args.status_file, state="failed", campaign="kv-quick-stock", run_id=args.run_id, error=f"benchmark binary SHA-256 mismatch: {args.bench_bin}")
                    return 2
                binary_verified = True

            env = os.environ.copy()
            env.update(
                {
                    "RUN_ID": args.run_id,
                    "BENCH_BIN": str(args.bench_bin),
                    "MATRIX_RESUME_SHUFFLE_REMAINING": "1",
                }
            )
            write_status(args.status_file, state="running", campaign="kv-quick-stock", run_id=args.run_id, busy_events=busy_events)
            proc = subprocess.run(
                [str(args.repo / "scripts" / "run-kv-matrix.sh"), "quick"],
                cwd=args.repo,
                env=env,
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            if proc.returncode == 0:
                if proc.stdout:
                    print(proc.stdout, end="", flush=True)
                if not complete(args.repo, args.run_id):
                    write_status(args.status_file, state="failed", campaign="kv-quick-stock", run_id=args.run_id, error="runner success did not validate")
                    return 2
                try:
                    audit(args.repo, args.run_id, args.audit_dir, args.audit_json, args.audit_markdown)
                except RuntimeError as error:
                    write_status(args.status_file, state="audit-failed", campaign="kv-quick-stock", run_id=args.run_id, error=str(error))
                    print(error, flush=True)
                    return 2
                write_status(args.status_file, state="complete", campaign="kv-quick-stock", run_id=args.run_id, busy_events=busy_events)
                print(f"KV quick complete and audited: {args.run_id}", flush=True)
                return 0
            if proc.returncode != 75:
                if proc.stdout:
                    print(proc.stdout, end="", flush=True)
                write_status(args.status_file, state="failed", campaign="kv-quick-stock", run_id=args.run_id, returncode=proc.returncode, busy_events=busy_events)
                return proc.returncode

            busy_events += 1
            write_status(args.status_file, state="waiting-for-idle", campaign="kv-quick-stock", run_id=args.run_id, busy_events=busy_events)
            if not args.watch:
                return 75
            if busy_events == 1 or busy_events % 30 == 0:
                print(f"KV quick yielded after host became busy: busy_events={busy_events}", flush=True)
            time.sleep(max(args.busy_sleep, 0.1))


if __name__ == "__main__":
    raise SystemExit(main())
