#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path


def validate_complete(run_dir: Path) -> tuple[dict, dict]:
    support_path = run_dir / "support.json"
    summary_path = run_dir / "summary.json"
    if not support_path.is_file() or not summary_path.is_file():
        raise RuntimeError(f"run is not complete: missing support/summary: {run_dir}")
    support = json.loads(support_path.read_text())
    summary = json.loads(summary_path.read_text())
    expected_rows = int(support["case_count"])
    trials = int(support["trials"])
    expected_groups = expected_rows // trials
    if summary.get("row_count") != expected_rows:
        raise RuntimeError(f"row-count mismatch for {run_dir}: {summary.get('row_count')} != {expected_rows}")
    if summary.get("group_count") != expected_groups:
        raise RuntimeError(f"group-count mismatch for {run_dir}: {summary.get('group_count')} != {expected_groups}")
    if summary.get("problems"):
        raise RuntimeError(f"summary problems remain for {run_dir}: {summary['problems']}")
    failures = run_dir / "failures.ndjson"
    if failures.is_file() and failures.read_text().strip():
        raise RuntimeError(f"runner failures remain for {run_dir}")
    return support, summary


def finalize(code_repo: Path, results_repo: Path, run_id: str) -> None:
    run_dir = results_repo / "results" / "runs" / run_id
    validate_complete(run_dir)
    subprocess.run(
        [
            sys.executable,
            str(code_repo / "scripts" / "summarize-concurrency-scaling.py"),
            str(run_dir),
            "--json-out", str(run_dir / "scaling.json"),
            "--markdown-out", str(run_dir / "scaling.md"),
        ],
        cwd=code_repo,
        check=True,
    )
    report = json.loads((run_dir / "scaling.json").read_text())
    if report.get("problems"):
        raise RuntimeError(f"scaling report problems remain for {run_id}: {report['problems']}")
    if report.get("incomplete_family_count"):
        raise RuntimeError(f"incomplete scaling families remain for {run_id}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Fail-closed finalizer for paired KV/record concurrency campaigns")
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--results-repo", type=Path, required=True)
    parser.add_argument("--kv-run-id", required=True)
    parser.add_argument("--record-run-id", required=True)
    args = parser.parse_args()
    for run_id in (args.kv_run_id, args.record_run_id):
        finalize(args.repo, args.results_repo, run_id)
    print(f"concurrency campaign finalized: kv={args.kv_run_id} record={args.record_run_id}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
