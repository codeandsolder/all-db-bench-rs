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
    expected_rows: int,
    expected_groups: int,
) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    ndjson = out_dir / "results.ndjson"
    manifest = out_dir / "manifest.json"
    summary = out_dir / "summary.json"
    subprocess.run(
        [
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
        ],
        cwd=repo,
        check=True,
    )
    subprocess.run(
        [
            sys.executable,
            str(repo / "scripts" / "summarize.py"),
            str(ndjson),
            "--expect-trials",
            "3",
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
    if manifest_data.get("selected_rows") != expected_rows or manifest_data.get("selected_groups") != expected_groups:
        raise RuntimeError(f"selection cardinality mismatch: {out_dir}")
    if summary_data.get("row_count") != expected_rows or summary_data.get("group_count") != expected_groups or summary_data.get("problems"):
        raise RuntimeError(f"summary validation failed: {out_dir}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Build strict final raw-KV and record baseline corpora after sizing follow-ups")
    parser.add_argument("--repo", type=Path, required=True)
    args = parser.parse_args()
    repo = args.repo

    finalize_one(
        repo,
        stock_results=Path("/srv/scratch/db-bench-work/kv-sizing-v3/results/runs/20261006-kv-quick-67228ce/results.ndjson"),
        audit=Path("/srv/scratch/db-bench-work/kv-sizing-audit/20261006-kv-quick-67228ce.json"),
        resize_prefix="20261006-kv-resize",
        out_dir=Path("/srv/scratch/db-bench-work/kv-sizing-audit/selected-final"),
        expected_rows=540,
        expected_groups=180,
    )
    finalize_one(
        repo,
        stock_results=repo / "results" / "runs" / "20261006-record-quick-stock" / "results.ndjson",
        audit=Path("/srv/scratch/db-bench-work/record-sizing-audit/20261006-record-quick-stock.json"),
        resize_prefix="20261006-record-resize",
        out_dir=Path("/srv/scratch/db-bench-work/record-sizing-audit/selected-final"),
        expected_rows=120,
        expected_groups=40,
    )
    print("final baseline corpora validated: kv=540/180 record=120/40", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
