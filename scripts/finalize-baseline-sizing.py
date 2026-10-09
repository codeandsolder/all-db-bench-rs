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


def finalize_one(
    repo: Path,
    *,
    stock_results: Path,
    audit: Path,
    resize_prefix: str,
    out_dir: Path,
    expected_groups: int,
    quality_repair_plan: Path | None = None,
) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    ndjson = out_dir / "results.ndjson"
    manifest = out_dir / "manifest.json"
    summary = out_dir / "summary.json"
    command = [
        sys.executable,
        str(repo / "scripts" / "select-baseline-sizing-results.py"),
        str(stock_results),
        str(audit),
        str(repo / "results" / "runs"),
        "--resize-prefix",
        resize_prefix,
        "--ndjson-out",
        str(ndjson),
        "--manifest-out",
        str(manifest),
    ]
    if quality_repair_plan is not None:
        command.extend(["--quality-repair-plan", str(quality_repair_plan)])
    subprocess.run(command, cwd=repo, check=True)
    subprocess.run(
        [
            sys.executable,
            str(repo / "scripts" / "summarize.py"),
            str(ndjson),
            "--json-out",
            str(summary),
            "--markdown-out",
            str(out_dir / "summary.md"),
        ],
        cwd=repo,
        check=True,
    )
    manifest_data = json.loads(manifest.read_text())
    summary_data = json.loads(summary.read_text())
    if not manifest_data.get("complete"):
        raise RuntimeError(f"selection remained incomplete: {out_dir}")
    selected_rows = int(manifest_data.get("selected_rows", -1))
    if manifest_data.get("selected_groups") != expected_groups:
        raise RuntimeError(f"selection group-count mismatch: {out_dir}")
    if (
        summary_data.get("row_count") != selected_rows
        or summary_data.get("group_count") != expected_groups
        or summary_data.get("problems")
    ):
        raise RuntimeError(f"summary validation failed: {out_dir}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Build strict final raw-KV and record baseline corpora after sizing follow-ups")
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--record-only", action="store_true")
    parser.add_argument("--record-stock-results", type=Path, required=True)
    parser.add_argument("--record-audit", type=Path, required=True)
    parser.add_argument("--record-resize-prefix", default="20261009-record-full-v1-resize-v1")
    parser.add_argument("--record-out-dir", type=Path, default=Path("/srv/scratch/db-bench-work/record-full-v1/selected-final-v1"))
    parser.add_argument("--record-quality-repair-plan", type=Path)
    parser.add_argument("--record-skip-quality-repairs", action="store_true")
    args = parser.parse_args()
    repo = args.repo

    if not args.record_only:
        finalize_one(
            repo,
            stock_results=Path("/srv/scratch/db-bench-work/kv-sizing-v3/results/runs/20261006-kv-quick-67228ce/results.ndjson"),
            audit=Path("/srv/scratch/db-bench-work/kv-sizing-audit/20261006-kv-quick-67228ce-v4.json"),
            resize_prefix="20261006-kv-resize-v4",
            out_dir=Path("/srv/scratch/db-bench-work/kv-sizing-audit/selected-final-v4"),
            expected_groups=180,
            quality_repair_plan=Path("/srv/scratch/db-bench-work/kv-sizing-audit/20261006-kv-stock-pressure-repairs-refined-v1.json"),
        )

    record_stock_results = args.record_stock_results
    record_audit = args.record_audit
    record_quality_repair_plan = (
        None if args.record_skip_quality_repairs else args.record_quality_repair_plan
    )
    finalize_one(
        repo,
        stock_results=record_stock_results,
        audit=record_audit,
        resize_prefix=args.record_resize_prefix,
        out_dir=args.record_out_dir,
        expected_groups=40,
        quality_repair_plan=record_quality_repair_plan,
    )
    print(
        "final baseline corpora validated: "
        + ("record=40 groups" if args.record_only else "kv=180 groups record=40 groups"),
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
