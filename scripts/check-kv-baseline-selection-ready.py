#!/usr/bin/env -S uv run --script
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def validate(selected: Path, *, expected_rows: int, expected_groups: int, expected_repairs: int) -> dict:
    results_path = selected / "results.ndjson"
    manifest_path = selected / "manifest.json"
    complete_path = selected / "complete.json"
    for path in (results_path, manifest_path, complete_path):
        if not path.is_file():
            raise ValueError(f"missing v5 selection artifact: {path}")
    completion = json.loads(complete_path.read_text())
    manifest = json.loads(manifest_path.read_text())
    row_count = sum(1 for line in results_path.read_text().splitlines() if line.strip())
    if completion.get("completion_version") != 1 or completion.get("selection_version") != 5:
        raise ValueError("unexpected v5 completion identity")
    if not completion.get("admission_repair_complete") or not manifest.get("admission_repair_complete"):
        raise ValueError("v5 admission repair is not complete")
    if int(completion.get("selected_rows", -1)) != expected_rows or row_count != expected_rows:
        raise ValueError("v5 row count mismatch")
    if int(completion.get("selected_groups", -1)) != expected_groups or int(manifest.get("selected_groups", -1)) != expected_groups:
        raise ValueError("v5 group count mismatch")
    repaired = manifest.get("repaired_groups")
    if not isinstance(repaired, list) or len(repaired) != expected_repairs or int(completion.get("repaired_groups", -1)) != expected_repairs:
        raise ValueError("v5 repaired-group count mismatch")
    if int(manifest.get("final_status_counts", {}).get("undersized", 0)) != 0:
        raise ValueError("v5 selection still contains undersized groups")
    if int(manifest.get("selected_followup_rejected_pressure_attempts", -1)) != 0:
        raise ValueError("v5 selection retained pressure-rejected follow-up attempts")
    if not manifest.get("repair_case_min_free_gib") or manifest.get("repair_initial_min_free_gib") is None:
        raise ValueError("v5 repair free-space provenance is missing")
    if completion.get("results_sha256") != sha256(results_path):
        raise ValueError("v5 results hash mismatch")
    if completion.get("manifest_sha256") != sha256(manifest_path):
        raise ValueError("v5 manifest hash mismatch")
    if completion.get("repair_plan_sha256") != manifest.get("repair_plan_sha256"):
        raise ValueError("v5 repair-plan provenance mismatch")
    return {
        "selected_rows": expected_rows,
        "selected_groups": expected_groups,
        "repaired_groups": expected_repairs,
        "results_sha256": completion["results_sha256"],
        "manifest_sha256": completion["manifest_sha256"],
    }


def main() -> int:
    ap = argparse.ArgumentParser(description="Verify transactional selected-final-v5 completion")
    ap.add_argument("selected", type=Path)
    ap.add_argument("--expected-rows", type=int, default=1046)
    ap.add_argument("--expected-groups", type=int, default=180)
    ap.add_argument("--expected-repairs", type=int, default=5)
    args = ap.parse_args()
    try:
        report = validate(args.selected, expected_rows=args.expected_rows, expected_groups=args.expected_groups, expected_repairs=args.expected_repairs)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        raise SystemExit(str(error)) from error
    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
