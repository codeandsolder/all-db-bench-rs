#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
import os
import shutil
from pathlib import Path
from typing import Any


def available_cpus() -> int:
    try:
        return max(1, len(os.sched_getaffinity(0)))
    except (AttributeError, OSError):
        return max(1, os.cpu_count() or 1)


def pressure_evidence(
    row: dict[str, Any],
    *,
    max_elapsed_s: float,
    max_runqueue_fraction: float,
    max_cpu_psi_fraction: float,
    max_benchmark_cpu_share: float,
    cpu_count: int,
) -> dict[str, Any] | None:
    elapsed = float(row.get("elapsed_s", 0.0))
    if elapsed <= 0.0 or elapsed > max_elapsed_s:
        return None

    measured_process = row.get("measured_process") or {}
    measured_system = row.get("measured_system_delta") or {}
    cpu_fraction = float(measured_process.get("cpu_runtime_fraction_of_wall", 0.0))
    max_benchmark_cpu_fraction = max(1.0, cpu_count * max_benchmark_cpu_share)
    # If the benchmark itself occupies most of the host, CPU PSI/runqueue wait
    # can be self-induced and is not safe evidence of external contention.
    if cpu_fraction > max_benchmark_cpu_fraction:
        return None

    runqueue_fraction = float(measured_process.get("runqueue_wait_fraction_of_wall", 0.0))
    accounting_wall_ns = float(measured_system.get("accounting_wall_ns", 0.0))
    if accounting_wall_ns <= 0.0:
        accounting_wall_ns = elapsed * 1_000_000_000.0
    cpu_psi_us = float(measured_system.get("psi_cpu_some_us", 0.0))
    cpu_psi_fraction = cpu_psi_us / max(accounting_wall_ns / 1_000.0, 1.0)

    reasons: list[str] = []
    if runqueue_fraction > max_runqueue_fraction:
        reasons.append("runqueue")
    if cpu_psi_fraction > max_cpu_psi_fraction:
        reasons.append("cpu-psi")
    if not reasons:
        return None
    return {
        "reasons": reasons,
        "elapsed_s": elapsed,
        "runqueue_wait_fraction_of_wall": runqueue_fraction,
        "cpu_psi_some_fraction_of_wall": cpu_psi_fraction,
        "benchmark_cpu_runtime_fraction_of_wall": cpu_fraction,
        "max_benchmark_cpu_fraction": max_benchmark_cpu_fraction,
        "write_bytes": int(measured_process.get("write_bytes", 0)),
    }


def next_attempt_dir(root: Path, case_id: str) -> Path:
    case_root = root / case_id
    case_root.mkdir(parents=True, exist_ok=True)
    existing = [p for p in case_root.glob("attempt-*") if p.is_dir()]
    attempt = 1
    if existing:
        attempt = max(int(p.name.split("-", 1)[1]) for p in existing) + 1
    target = case_root / f"attempt-{attempt:03d}"
    target.mkdir()
    return target


def archive_case(run_dir: Path, case_path: Path, evidence: dict[str, Any]) -> Path:
    case_id = case_path.stem
    target = next_attempt_dir(run_dir / "rejected-pressure", case_id)
    shutil.move(str(case_path), target / "case.json")
    companions = (
        (run_dir / "noise" / f"{case_id}.before.json", "noise-before.json"),
        (run_dir / "noise" / f"{case_id}.after.json", "noise-after.json"),
        (run_dir / "stderr" / f"{case_id}.log", "stderr.log"),
    )
    for source, name in companions:
        if source.exists():
            shutil.move(str(source), target / name)
    (target / "rejection.json").write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    return target


def scrub(
    run_dir: Path,
    *,
    max_elapsed_s: float = 0.5,
    max_runqueue_fraction: float = 0.03,
    max_cpu_psi_fraction: float = 0.05,
    max_benchmark_cpu_share: float = 0.5,
    dry_run: bool = False,
    cpu_count: int | None = None,
) -> dict[str, Any]:
    cpu_count = available_cpus() if cpu_count is None else max(1, cpu_count)
    cases_dir = run_dir / "cases"
    rejected: list[dict[str, Any]] = []
    examined = 0
    if cases_dir.is_dir():
        for case_path in sorted(cases_dir.glob("*.json")):
            row = json.loads(case_path.read_text())
            examined += 1
            evidence = pressure_evidence(
                row,
                max_elapsed_s=max_elapsed_s,
                max_runqueue_fraction=max_runqueue_fraction,
                max_cpu_psi_fraction=max_cpu_psi_fraction,
                max_benchmark_cpu_share=max_benchmark_cpu_share,
                cpu_count=cpu_count,
            )
            if evidence is None:
                continue
            evidence = {
                "case_id": case_path.stem,
                "trial": row.get("trial"),
                "engine": row.get("engine"),
                "durability": row.get("durability"),
                "workload": row.get("workload"),
                "thresholds": {
                    "max_elapsed_s": max_elapsed_s,
                    "max_runqueue_fraction": max_runqueue_fraction,
                    "max_cpu_psi_fraction": max_cpu_psi_fraction,
                    "max_benchmark_cpu_share": max_benchmark_cpu_share,
                    "available_cpus": cpu_count,
                },
                **evidence,
            }
            if not dry_run:
                target = archive_case(run_dir, case_path, evidence)
                evidence["archive"] = str(target)
            rejected.append(evidence)

    if rejected and not dry_run:
        # These are derived from the accepted case set and become stale as
        # soon as one case is archived. The unchanged matrix runner rebuilds
        # them after filling the missing trial.
        for name in ("results.ndjson", "summary.json", "summary.md", "host-end.txt"):
            path = run_dir / name
            if path.exists():
                path.unlink()

    return {
        "run_dir": str(run_dir),
        "examined": examined,
        "rejected": len(rejected),
        "dry_run": dry_run,
        "available_cpus": cpu_count,
        "thresholds": {
            "max_elapsed_s": max_elapsed_s,
            "max_runqueue_fraction": max_runqueue_fraction,
            "max_cpu_psi_fraction": max_cpu_psi_fraction,
            "max_benchmark_cpu_share": max_benchmark_cpu_share,
        },
        "cases": rejected,
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Archive very short benchmark trials with in-window CPU-pressure evidence"
    )
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("--max-elapsed-s", type=float, default=0.5)
    parser.add_argument("--max-runqueue-fraction", type=float, default=0.03)
    parser.add_argument("--max-cpu-psi-fraction", type=float, default=0.05)
    parser.add_argument("--max-benchmark-cpu-share", type=float, default=0.5)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    report = scrub(
        args.run_dir,
        max_elapsed_s=args.max_elapsed_s,
        max_runqueue_fraction=args.max_runqueue_fraction,
        max_cpu_psi_fraction=args.max_cpu_psi_fraction,
        max_benchmark_cpu_share=args.max_benchmark_cpu_share,
        dry_run=args.dry_run,
    )
    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
