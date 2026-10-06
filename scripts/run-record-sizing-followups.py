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
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, TextIO

DEFAULT_REPO = Path("/srv/scratch/db-bench-work/kv-sizing-v3-followup")
DEFAULT_PLAN = Path("/srv/scratch/db-bench-work/record-sizing-audit/20261006-record-quick-stock-v4.json")
DEFAULT_BIN = Path("/srv/scratch/db-bench-work/record-sizing-audit/bin/recordbench-ae74103b847171d1")
DEFAULT_ROCKS_BIN = Path("/srv/scratch/db-bench-work/record-sizing-audit/bin/surrealdb-rocksdb-recordbench-c3978a3b66a24edd")
DEFAULT_LOCK = Path("/run/lock/all-db-bench-performance.lock")
DEFAULT_STATUS = Path("/srv/scratch/db-bench-work/record-sizing-audit/resize-idle-status.json")


def slug(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "-_" else "-" for ch in value)


def run_id(group: dict[str, Any]) -> str:
    return (
        "20261006-record-resize-v4-"
        f"{slug(str(group['engine']))}-{slug(str(group['durability']))}-"
        f"{slug(str(group['workload']))}-e{int(group['suggested_effective_ops'])}"
        f"-t{int(group['suggested_trials'])}"
    )


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def complete(repo: Path, group: dict[str, Any]) -> bool:
    summary = repo / "results" / "runs" / run_id(group) / "summary.json"
    if not summary.is_file():
        return False
    try:
        data = json.loads(summary.read_text())
    except (json.JSONDecodeError, OSError):
        return False
    expected_trials = int(group["suggested_trials"])
    if data.get("row_count") != expected_trials or data.get("group_count") != 1 or data.get("problems"):
        return False
    groups = data.get("groups")
    if not isinstance(groups, list) or len(groups) != 1:
        return False
    result = groups[0]
    return (
        result.get("engine") == group.get("engine")
        and result.get("durability") == group.get("durability")
        and result.get("workload") == group.get("workload")
        and int(result.get("ops_requested", -1)) == int(group["suggested_effective_ops"])
        and result.get("trials") == list(range(1, expected_trials + 1))
    )


def median_elapsed(repo: Path, group: dict[str, Any]) -> float:
    case_dir = repo / "results" / "runs" / run_id(group) / "cases"
    values: list[float] = []
    for path in sorted(case_dir.glob("*.json")):
        row = json.loads(path.read_text())
        if (
            row.get("engine") == group.get("engine")
            and row.get("durability") == group.get("durability")
            and row.get("workload") == group.get("workload")
            and int(row.get("ops_requested", -1)) == int(group["suggested_effective_ops"])
        ):
            values.append(float(row["elapsed_s"]))
    expected_trials = int(group["suggested_trials"])
    if len(values) != expected_trials:
        raise RuntimeError(
            f"expected {expected_trials} validated trials for {run_id(group)}, found {len(values)}"
        )
    return statistics.median(values)


def calibration_elapsed(
    repo: Path,
    group: dict[str, Any],
    *,
    index: int,
    count: int,
    minimum: float,
    maximum: float,
) -> float | None:
    if group.get("resize_strategy") != "more-ops" or index > count:
        return None
    elapsed = median_elapsed(repo, group)
    if not minimum <= elapsed <= maximum:
        raise RuntimeError(
            f"record resize calibration outside [{minimum:.3f}, {maximum:.3f}] s for {run_id(group)}: "
            f"median_elapsed_s={elapsed:.3f}"
        )
    return elapsed


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


def scrub_short_pressure(repo: Path, run_dir: Path) -> int:
    proc = subprocess.run(
        [sys.executable, str(repo / "scripts" / "scrub-short-trial-pressure.py"), str(run_dir)],
        cwd=repo,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"short-trial pressure scrub failed rc={proc.returncode}: {proc.stderr.strip()}"
        )
    try:
        report = json.loads(proc.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"invalid short-trial pressure scrub output: {proc.stdout!r}") from error
    rejected = int(report.get("rejected", 0))
    if rejected:
        case_ids = ",".join(str(item.get("case_id")) for item in report.get("cases", []))
        print(f"archived {rejected} transient-pressure trial(s): {case_ids}", flush=True)
    return rejected


def groups_from_plan(path: Path) -> list[dict[str, Any]]:
    plan = json.loads(path.read_text())
    if plan.get("sizing_policy_version") != 2:
        raise ValueError(f"unsupported sizing policy: {plan.get('sizing_policy_version')!r}")
    groups = [group for group in plan["groups"] if group["status"] == "undersized"]
    groups.sort(
        key=lambda group: (
            0 if group.get("resize_strategy") == "more-ops" else 1,
            float(group["median_elapsed_s"]),
            str(group["engine"]),
            str(group["durability"]),
            str(group["workload"]),
        )
    )
    return groups


def command_env(group: dict[str, Any], bench_bin: Path, rocks_bench_bin: Path) -> dict[str, str]:
    env = os.environ.copy()
    env.update(
        {
            "RUN_ID": run_id(group),
            "BENCH_BIN": str(bench_bin),
            "ROCKS_BENCH_BIN": str(rocks_bench_bin),
            "ENGINES_OVERRIDE": str(group["engine"]),
            "DURABILITIES_OVERRIDE": str(group["durability"]),
            "WORKLOADS_OVERRIDE": str(group["workload"]),
            "RECORD_OPS_OVERRIDE": str(int(group["runner_ops_override"])),
            "RECORD_RECORDS_OVERRIDE": str(int(group["records"])),
            "RECORD_TRIALS_OVERRIDE": str(int(group["suggested_trials"])),
        }
    )
    return env


def main() -> int:
    parser = argparse.ArgumentParser(description="Opportunistically resize undersized record-product quick groups")
    parser.add_argument("--repo", type=Path, default=DEFAULT_REPO)
    parser.add_argument("--plan", type=Path, default=DEFAULT_PLAN)
    parser.add_argument("--bench-bin", type=Path, default=DEFAULT_BIN)
    parser.add_argument("--rocks-bench-bin", type=Path, default=DEFAULT_ROCKS_BIN)
    parser.add_argument("--expected-bench-sha256")
    parser.add_argument("--expected-rocks-bench-sha256")
    parser.add_argument("--lock-file", type=Path, default=DEFAULT_LOCK)
    parser.add_argument("--status-file", type=Path, default=DEFAULT_STATUS)
    parser.add_argument("--limit", type=int)
    parser.add_argument("--watch", action="store_true")
    parser.add_argument("--busy-sleep", type=float, default=2.0)
    parser.add_argument("--max-io-full-avg10", type=float, default=5.0)
    parser.add_argument("--calibration-count", type=int, default=5)
    parser.add_argument("--calibration-min-seconds", type=float, default=1.5)
    parser.add_argument("--calibration-max-seconds", type=float, default=15.0)
    args = parser.parse_args()

    if not args.plan.is_file():
        parser.error(f"plan does not exist: {args.plan}")
    for path, expected in (
        (args.bench_bin, args.expected_bench_sha256),
        (args.rocks_bench_bin, args.expected_rocks_bench_sha256),
    ):
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error(f"benchmark binary is not executable: {path}")

    try:
        lock_handle = acquire_lock(args.lock_file)
    except RuntimeError as error:
        print(error, flush=True)
        return 73

    with lock_handle:
        groups = groups_from_plan(args.plan)
        if args.limit is not None:
            groups = groups[: args.limit]
        total = len(groups)
        print(f"record resize planned={total}", flush=True)
        completed_now = 0
        skipped = 0
        busy_events = 0
        binaries_verified = not bool(args.expected_bench_sha256 or args.expected_rocks_bench_sha256)

        for index, group in enumerate(groups, 1):
            rid = run_id(group)
            run_dir = args.repo / "results" / "runs" / rid
            if group.get("resize_strategy") == "more-trials":
                try:
                    scrub_short_pressure(args.repo, run_dir)
                except RuntimeError as error:
                    write_status(args.status_file, state="failed", campaign="record-sizing-followups", current_run_id=rid, error=str(error))
                    print(error, flush=True)
                    return 2
            if complete(args.repo, group):
                skipped += 1
                elapsed = calibration_elapsed(
                    args.repo,
                    group,
                    index=index,
                    count=args.calibration_count,
                    minimum=args.calibration_min_seconds,
                    maximum=args.calibration_max_seconds,
                )
                if elapsed is not None:
                    print(f"record calibration {index}/{args.calibration_count}: median_elapsed_s={elapsed:.3f}", flush=True)
                continue

            print(
                f"[{index}/{total}] {rid} strategy={group['resize_strategy']} "
                f"effective_ops={int(group['suggested_effective_ops'])} "
                f"trials={int(group['suggested_trials'])} runner_ops={int(group['runner_ops_override'])}",
                flush=True,
            )
            while True:
                preflight_rc = preflight_host(args.repo, max_io_full_avg10=args.max_io_full_avg10)
                if preflight_rc == 75:
                    busy_events += 1
                    write_status(
                        args.status_file,
                        state="waiting-for-idle",
                        campaign="record-sizing-followups",
                        current_index=index,
                        total=total,
                        current_run_id=rid,
                        completed_now=completed_now,
                        skipped=skipped,
                        busy_events=busy_events,
                    )
                    if not args.watch:
                        return 75
                    if busy_events == 1 or busy_events % 30 == 0:
                        print(f"record resize waiting for idle: run={rid} busy_events={busy_events}", flush=True)
                    time.sleep(max(args.busy_sleep, 0.1))
                    continue
                if preflight_rc != 0:
                    return preflight_rc

                if not binaries_verified:
                    checks = ((args.bench_bin, args.expected_bench_sha256), (args.rocks_bench_bin, args.expected_rocks_bench_sha256))
                    for path, expected in checks:
                        if expected and sha256(path) != expected:
                            write_status(args.status_file, state="failed", campaign="record-sizing-followups", current_run_id=rid, error=f"benchmark binary SHA-256 mismatch: {path}")
                            return 2
                    binaries_verified = True

                write_status(
                    args.status_file,
                    state="running",
                    campaign="record-sizing-followups",
                    current_index=index,
                    total=total,
                    current_run_id=rid,
                    completed_now=completed_now,
                    skipped=skipped,
                    busy_events=busy_events,
                )
                proc = subprocess.run(
                    [str(args.repo / "scripts" / "run-record-matrix.sh"), "quick"],
                    cwd=args.repo,
                    env=command_env(group, args.bench_bin, args.rocks_bench_bin),
                    check=False,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                )
                scrubbed = 0
                if group.get("resize_strategy") == "more-trials":
                    try:
                        scrubbed = scrub_short_pressure(args.repo, run_dir)
                    except RuntimeError as error:
                        write_status(args.status_file, state="failed", campaign="record-sizing-followups", current_run_id=rid, error=str(error))
                        print(error, flush=True)
                        return 2
                if proc.returncode == 0:
                    if proc.stdout:
                        print(proc.stdout, end="", flush=True)
                    if scrubbed:
                        continue
                    if not complete(args.repo, group):
                        print(f"record runner returned success but result validation failed: {rid}", flush=True)
                        return 2
                    completed_now += 1
                    break
                if proc.returncode != 75:
                    if proc.stdout:
                        print(proc.stdout, end="", flush=True)
                    write_status(
                        args.status_file,
                        state="failed",
                        campaign="record-sizing-followups",
                        current_index=index,
                        total=total,
                        current_run_id=rid,
                        returncode=proc.returncode,
                        completed_now=completed_now,
                        skipped=skipped,
                        busy_events=busy_events,
                    )
                    return proc.returncode
                busy_events += 1
                write_status(
                    args.status_file,
                    state="waiting-for-idle",
                    campaign="record-sizing-followups",
                    current_index=index,
                    total=total,
                    current_run_id=rid,
                    completed_now=completed_now,
                    skipped=skipped,
                    busy_events=busy_events,
                )
                if not args.watch:
                    return 75
                if busy_events == 1 or busy_events % 30 == 0:
                    print(f"record resize yielded after host became busy: run={rid} busy_events={busy_events}", flush=True)
                time.sleep(max(args.busy_sleep, 0.1))

            try:
                elapsed = calibration_elapsed(
                    args.repo,
                    group,
                    index=index,
                    count=args.calibration_count,
                    minimum=args.calibration_min_seconds,
                    maximum=args.calibration_max_seconds,
                )
            except RuntimeError as error:
                write_status(
                    args.status_file,
                    state="calibration-failed",
                    campaign="record-sizing-followups",
                    current_index=index,
                    total=total,
                    current_run_id=rid,
                    error=str(error),
                    completed_now=completed_now,
                    skipped=skipped,
                    busy_events=busy_events,
                )
                print(error, flush=True)
                return 2
            if elapsed is not None:
                print(f"record calibration {index}/{args.calibration_count}: median_elapsed_s={elapsed:.3f}", flush=True)

        write_status(
            args.status_file,
            state="complete",
            campaign="record-sizing-followups",
            total=total,
            completed_now=completed_now,
            skipped=skipped,
            busy_events=busy_events,
        )
        print(f"record resize complete: completed_now={completed_now} skipped={skipped} total={total}", flush=True)
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
