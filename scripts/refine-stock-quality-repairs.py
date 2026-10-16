#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import importlib.util
import json
import sys
from pathlib import Path
from typing import Any


def _load_sizing_module():
    path = Path(__file__).with_name("audit_baseline_sizing.py")
    spec = importlib.util.spec_from_file_location("audit_baseline_sizing_for_quality_refine", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load sizing auditor: {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


SIZING = _load_sizing_module()


def slug(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "-_" else "-" for ch in value)


def run_id(group: dict[str, Any], prefix: str) -> str:
    return (
        f"{prefix}-{slug(str(group['engine']))}-{slug(str(group['durability']))}-"
        f"{slug(str(group['workload']))}-e{int(group['suggested_effective_ops'])}"
        f"-t{int(group['suggested_trials'])}"
    )


def read_ndjson(path: Path) -> list[dict[str, Any]]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def audit_clean_repair(rows: list[dict[str, Any]], sizing_audit: dict[str, Any], *, expect_trials: int) -> dict[str, Any]:
    thresholds = sizing_audit["thresholds"]
    report = SIZING.audit_rows(
        rows,
        expect_trials=expect_trials,
        read_only_min_seconds=float(thresholds["read_only_min_seconds"]),
        read_only_target_seconds=float(thresholds["read_only_target_seconds"]),
        stateful_min_total_seconds=float(thresholds["stateful_min_total_seconds"]),
        stateful_target_total_seconds=float(thresholds["stateful_target_total_seconds"]),
        stateful_max_trials=int(thresholds["stateful_max_trials"]),
        max_cv=float(thresholds["max_cv"]),
        max_relative_spread=float(thresholds["max_relative_spread"]),
    )
    if report["problems"] or report["group_count"] != 1:
        raise ValueError(f"clean repair audit failed: {report['problems']}")
    return report["groups"][0]


def refine_plan(
    quality_plan: dict[str, Any],
    sizing_audit: dict[str, Any],
    resize_root: Path,
    *,
    resize_prefix: str,
) -> dict[str, Any]:
    if quality_plan.get("quality_policy_version") != 1:
        raise ValueError(f"unsupported quality policy: {quality_plan.get('quality_policy_version')!r}")
    if int(sizing_audit.get("sizing_policy_version", -1)) not in {2, 3}:
        raise ValueError(f"unsupported sizing policy: {sizing_audit.get('sizing_policy_version')!r}")

    refined: list[dict[str, Any]] = []
    promoted = 0
    for original in quality_plan.get("groups", []):
        initial_rid = run_id(original, resize_prefix)
        run_dir = resize_root / initial_rid
        results_path = run_dir / "results.ndjson"
        summary_path = run_dir / "summary.json"
        if not results_path.is_file() or not summary_path.is_file():
            raise ValueError(f"missing initial quality-repair result: {initial_rid}")
        expected_trials = int(original["suggested_trials"])
        summary = json.loads(summary_path.read_text())
        if summary.get("row_count") != expected_trials or summary.get("group_count") != 1 or summary.get("problems"):
            raise ValueError(f"invalid initial quality-repair summary: {initial_rid}")
        audited = audit_clean_repair(read_ndjson(results_path), sizing_audit, expect_trials=expected_trials)

        item = dict(original)
        item["quality_repair_initial_run_id"] = initial_rid
        item["quality_repair_clean_status"] = audited["status"]
        item["quality_repair_clean_median_elapsed_s"] = audited["median_elapsed_s"]
        item["quality_repair_clean_measured_total_s"] = audited["measured_total_s"]
        item["quality_repair_clean_throughput_cv"] = audited["throughput_cv"]
        item["quality_repair_clean_throughput_relative_spread"] = audited["throughput_relative_spread"]
        item["quality_repair_refined"] = audited["status"] == "undersized"
        item["quality_repair_refinement_strategy"] = audited.get("resize_strategy")
        if audited["status"] == "undersized":
            if audited.get("sampling_cap_insufficient"):
                raise ValueError(f"quality repair cannot meet sampling floor within cap: {initial_rid}")
            item["suggested_effective_ops"] = int(audited["suggested_effective_ops"])
            item["runner_ops_override"] = int(audited["runner_ops_override"])
            item["suggested_trials"] = int(audited["suggested_trials"])
            promoted += 1
        refined.append(item)

    result = dict(quality_plan)
    result["refinement_version"] = 1
    result["source_quality_policy_version"] = quality_plan["quality_policy_version"]
    result["source_sizing_policy_version"] = sizing_audit["sizing_policy_version"]
    result["refined_group_count"] = promoted
    result["groups"] = refined
    return result


def render_markdown(plan: dict[str, Any], *, resize_prefix: str) -> str:
    lines = [
        "# Refined stock short-pressure repair plan",
        "",
        f"Repair groups: **{len(plan.get('groups', []))}**; promoted after clean re-audit: **{plan.get('refined_group_count', 0)}**.",
        "",
        "| engine | durability | workload | clean status | refinement | ops | trials | run id |",
        "| --- | --- | --- | --- | --- | ---: | ---: | --- |",
    ]
    for group in plan.get("groups", []):
        lines.append(
            f"| {group['engine']} | {group['durability']} | {group['workload']} | "
            f"{group.get('quality_repair_clean_status', '—')} | {group.get('quality_repair_refinement_strategy') or '—'} | "
            f"{int(group['suggested_effective_ops']):,} | {int(group['suggested_trials'])} | `{run_id(group, resize_prefix)}` |"
        )
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description="Re-audit clean stock-pressure repairs and promote any still-undersized group")
    parser.add_argument("quality_plan", type=Path)
    parser.add_argument("sizing_audit", type=Path)
    parser.add_argument("resize_root", type=Path)
    parser.add_argument("--resize-prefix", required=True)
    parser.add_argument("--json-out", type=Path, required=True)
    parser.add_argument("--markdown-out", type=Path)
    args = parser.parse_args()

    refined = refine_plan(
        json.loads(args.quality_plan.read_text()),
        json.loads(args.sizing_audit.read_text()),
        args.resize_root,
        resize_prefix=args.resize_prefix,
    )
    args.json_out.parent.mkdir(parents=True, exist_ok=True)
    args.json_out.write_text(json.dumps(refined, indent=2, sort_keys=True) + "\n")
    markdown = render_markdown(refined, resize_prefix=args.resize_prefix)
    if args.markdown_out:
        args.markdown_out.parent.mkdir(parents=True, exist_ok=True)
        args.markdown_out.write_text(markdown)
    else:
        print(markdown, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
