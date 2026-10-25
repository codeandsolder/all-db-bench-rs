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
            str(repo / "scripts" / "benchctl.sh"),
            "summarize",
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


def required_path(parser: argparse.ArgumentParser, value: Path | None, option: str) -> Path:
    if value is None:
        parser.error(f"{option} is required for the selected lane")
    return value


def main() -> int:
    parser = argparse.ArgumentParser(description="Build strict final raw-KV and/or record baseline corpora after sizing follow-ups")
    parser.add_argument("--repo", type=Path, required=True)
    lane = parser.add_mutually_exclusive_group()
    lane.add_argument("--kv-only", action="store_true")
    lane.add_argument("--record-only", action="store_true")

    parser.add_argument("--kv-stock-results", type=Path)
    parser.add_argument("--kv-audit", type=Path)
    parser.add_argument("--kv-resize-prefix", default="kv-resize")
    parser.add_argument("--kv-out-dir", type=Path)
    parser.add_argument("--kv-quality-repair-plan", type=Path)
    parser.add_argument("--kv-skip-quality-repairs", action="store_true")

    parser.add_argument("--record-stock-results", type=Path)
    parser.add_argument("--record-audit", type=Path)
    parser.add_argument("--record-resize-prefix", default="record-resize")
    parser.add_argument("--record-out-dir", type=Path)
    parser.add_argument("--record-quality-repair-plan", type=Path)
    parser.add_argument("--record-skip-quality-repairs", action="store_true")
    args = parser.parse_args()
    repo = args.repo

    run_kv = not args.record_only
    run_record = not args.kv_only

    if run_kv:
        kv_stock_results = required_path(parser, args.kv_stock_results, "--kv-stock-results")
        kv_audit = required_path(parser, args.kv_audit, "--kv-audit")
        kv_out_dir = required_path(parser, args.kv_out_dir, "--kv-out-dir")
        kv_quality_repair_plan = None if args.kv_skip_quality_repairs else args.kv_quality_repair_plan
        finalize_one(
            repo,
            stock_results=kv_stock_results,
            audit=kv_audit,
            resize_prefix=args.kv_resize_prefix,
            out_dir=kv_out_dir,
            expected_groups=180,
            quality_repair_plan=kv_quality_repair_plan,
        )

    if run_record:
        record_stock_results = required_path(parser, args.record_stock_results, "--record-stock-results")
        record_audit = required_path(parser, args.record_audit, "--record-audit")
        record_out_dir = required_path(parser, args.record_out_dir, "--record-out-dir")
        record_quality_repair_plan = None if args.record_skip_quality_repairs else args.record_quality_repair_plan
        finalize_one(
            repo,
            stock_results=record_stock_results,
            audit=record_audit,
            resize_prefix=args.record_resize_prefix,
            out_dir=record_out_dir,
            expected_groups=40,
            quality_repair_plan=record_quality_repair_plan,
        )

    lanes = []
    if run_kv:
        lanes.append("kv=180 groups")
    if run_record:
        lanes.append("record=40 groups")
    print("final baseline corpora validated: " + " ".join(lanes), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
