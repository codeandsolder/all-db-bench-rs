#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path
from typing import Any

QUALITY_POLICY_VERSION = 1


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def runner_ops_override(lane: str, workload: str, effective_ops: int) -> int:
    if workload != "tiny-txn":
        return effective_ops
    if lane == "kv":
        return effective_ops * 10
    if lane == "record":
        return effective_ops * 2
    raise ValueError(f"unsupported lane for tiny-txn repair: {lane!r}")


def build_plan(
    run_dir: Path,
    sizing_audit: dict[str, Any],
    scrub_report: dict[str, Any],
    *,
    lane: str,
) -> dict[str, Any]:
    if sizing_audit.get("sizing_policy_version") != 2:
        raise ValueError(f"unsupported sizing policy: {sizing_audit.get('sizing_policy_version')!r}")
    by_identity = {
        (str(group["engine"]), str(group["durability"]), str(group["workload"])): group
        for group in sizing_audit["groups"]
    }
    pressure_by_identity: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
    for case in scrub_report.get("cases", []):
        key = (str(case["engine"]), str(case["durability"]), str(case["workload"]))
        pressure_by_identity.setdefault(key, []).append(case)

    repairs: list[dict[str, Any]] = []
    ignored_undersized: list[dict[str, Any]] = []
    expect_trials = int(sizing_audit["thresholds"]["expect_trials"])
    for key, cases in sorted(pressure_by_identity.items()):
        group = by_identity.get(key)
        if group is None:
            raise ValueError(f"pressure case missing from sizing audit: {key}")
        if group["status"] == "undersized":
            ignored_undersized.append(
                {
                    "engine": key[0],
                    "durability": key[1],
                    "workload": key[2],
                    "pressure_trials": sorted(int(case["trial"]) for case in cases),
                }
            )
            continue
        effective_ops = int(group["ops_requested"])
        repairs.append(
            {
                **group,
                "quality_repair_required": True,
                "resize_strategy": "quality-repair",
                "suggested_effective_ops": effective_ops,
                "runner_ops_override": runner_ops_override(lane, str(group["workload"]), effective_ops),
                "suggested_trials": expect_trials,
                "pressure_trials": sorted(int(case["trial"]) for case in cases),
                "pressure_evidence": cases,
            }
        )

    results_path = run_dir / "results.ndjson"
    return {
        "quality_policy_version": QUALITY_POLICY_VERSION,
        "lane": lane,
        "source_run_dir": str(run_dir),
        "source_results_sha256": sha256(results_path),
        "source_sizing_policy_version": sizing_audit["sizing_policy_version"],
        "pressure_thresholds": scrub_report.get("thresholds", {}),
        "available_cpus": scrub_report.get("available_cpus"),
        "pressure_cases_total": int(scrub_report.get("rejected", 0)),
        "repair_group_count": len(repairs),
        "ignored_undersized_group_count": len(ignored_undersized),
        "ignored_undersized_groups": ignored_undersized,
        "groups": repairs,
    }


def render_markdown(plan: dict[str, Any]) -> str:
    lines = [
        "# Stock short-trial pressure repair plan",
        "",
        f"Quality policy v{plan['quality_policy_version']}; lane `{plan['lane']}`.",
        f"Pressure-hit stock cases: **{plan['pressure_cases_total']}**; retained groups requiring clean replacement: **{plan['repair_group_count']}**; pressure-hit groups already replaced by sizing: **{plan['ignored_undersized_group_count']}**.",
        "",
        "| engine | durability | workload | stock status | ops | trials | pressure stock trials |",
        "| --- | --- | --- | --- | ---: | ---: | --- |",
    ]
    for group in plan["groups"]:
        lines.append(
            f"| {group['engine']} | {group['durability']} | {group['workload']} | {group['status']} | "
            f"{int(group['ops_requested']):,} | {int(group['suggested_trials'])} | "
            f"{','.join(map(str, group['pressure_trials']))} |"
        )
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description="Plan clean replacements for retained stock groups with very-short CPU-pressure hits")
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("sizing_audit", type=Path)
    parser.add_argument("--lane", choices=("kv", "record"), required=True)
    parser.add_argument("--json-out", type=Path, required=True)
    parser.add_argument("--markdown-out", type=Path)
    args = parser.parse_args()

    scrubber = Path(__file__).with_name("scrub-short-trial-pressure.py")
    proc = subprocess.run(
        [sys.executable, str(scrubber), str(args.run_dir), "--dry-run"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if proc.returncode != 0:
        print(proc.stderr, file=sys.stderr, end="")
        return proc.returncode
    scrub_report = json.loads(proc.stdout)
    audit = json.loads(args.sizing_audit.read_text())
    plan = build_plan(args.run_dir, audit, scrub_report, lane=args.lane)
    args.json_out.parent.mkdir(parents=True, exist_ok=True)
    args.json_out.write_text(json.dumps(plan, indent=2, sort_keys=True) + "\n")
    markdown = render_markdown(plan)
    if args.markdown_out:
        args.markdown_out.parent.mkdir(parents=True, exist_ok=True)
        args.markdown_out.write_text(markdown)
    else:
        print(markdown, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
