#!/usr/bin/env -S uv run --script
from __future__ import annotations

import argparse
import hashlib
import json
import math
import statistics
from pathlib import Path
from typing import Any


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


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


def refine_plan(
    base_plan: dict[str, Any],
    *,
    base_plan_sha256: str,
    run_root: Path,
    thresholds: dict[str, Any],
    run_prefix: str,
) -> dict[str, Any]:
    groups = list(base_plan.get("groups", []))
    if not groups:
        raise ValueError("base final-confirmation plan has no groups")
    if base_plan.get("quality_policy_version") != 1:
        raise ValueError("unsupported base final-confirmation quality policy")

    observations: dict[tuple[str, str, str, int], dict[str, Any]] = {}
    result_hashes: dict[str, str] = {}
    undersized_families: set[tuple[str, str]] = set()

    for group in groups:
        rid = run_id(group, run_prefix)
        result_path = run_root / rid / "results.ndjson"
        summary_path = run_root / rid / "summary.json"
        if not result_path.is_file() or not summary_path.is_file():
            raise ValueError(f"missing base confirmation result: {rid}")
        expected_trials = int(group["suggested_trials"])
        summary = json.loads(summary_path.read_text())
        if int(summary.get("row_count", -1)) != expected_trials or int(summary.get("group_count", -1)) != 1 or summary.get("problems"):
            raise ValueError(f"invalid base confirmation summary: {rid}")
        rows = read_ndjson(result_path)
        if sorted(int(row["trial"]) for row in rows) != list(range(1, expected_trials + 1)):
            raise ValueError(f"base confirmation trial mismatch: {rid}")
        expected_ops = int(group["suggested_effective_ops"])
        actual_ops = {int(row["ops_requested"]) for row in rows}
        if actual_ops != {expected_ops}:
            raise ValueError(f"base confirmation op mismatch: {rid}: expected {expected_ops}, got {sorted(actual_ops)}")

        elapsed = [float(row["elapsed_s"]) for row in rows]
        rates = [float(row["ops_per_s"]) for row in rows]
        median_elapsed = statistics.median(elapsed)
        total_elapsed = sum(elapsed)
        median_rate = statistics.median(rates)
        workload = str(group["workload"])
        read_only = workload in set(thresholds["read_only_workloads"])
        undersized = (
            median_elapsed < float(thresholds["read_only_min_seconds"])
            if read_only
            else total_elapsed < float(thresholds["stateful_min_total_seconds"])
        )

        logical = (str(group["engine"]), str(group["durability"]), workload, int(group["records"]))
        if logical in observations:
            raise ValueError(f"duplicate logical identity: {logical}")
        observations[logical] = {
            "median_elapsed_s": median_elapsed,
            "total_elapsed_s": total_elapsed,
            "median_ops_per_s": median_rate,
            "undersized": undersized,
        }
        result_hashes[rid] = sha256(result_path)
        if undersized:
            undersized_families.add((str(group["durability"]), workload))

    common = dict(base_plan.get("common_effective_ops", {}))
    new_ops_by_family: dict[tuple[str, str], int] = {}
    refined_families: dict[str, dict[str, Any]] = {}
    sizing_rule = base_plan.get("sizing_rule")
    if not isinstance(sizing_rule, dict) or "slowest_ceiling_seconds" not in sizing_rule:
        raise ValueError("base final-confirmation plan is missing slowest_ceiling_seconds")
    slowest_ceiling_seconds = float(sizing_rule["slowest_ceiling_seconds"])
    parent_strategy = str(base_plan.get("strategy", ""))
    hard_min_safety_factor = 1.10 if parent_strategy == "final-five-trial-common-work-v5" else 1.0
    next_strategy = (
        "final-five-trial-common-work-v6"
        if hard_min_safety_factor > 1.0
        else "final-five-trial-common-work-v5"
    )

    for durability, workload in sorted(undersized_families):
        family_groups = [g for g in groups if str(g["durability"]) == durability and str(g["workload"]) == workload]
        old_ops_set = {int(g["suggested_effective_ops"]) for g in family_groups}
        if len(old_ops_set) != 1:
            raise ValueError(f"base family is not common-work: {durability}/{workload}")
        old_ops = next(iter(old_ops_set))
        rates = [
            float(observations[(str(g["engine"]), durability, workload, int(g["records"]))]["median_ops_per_s"])
            for g in family_groups
        ]
        fastest_rate = max(rates)
        slowest_rate = min(rates)
        trial_counts = {int(g["suggested_trials"]) for g in family_groups}
        if len(trial_counts) != 1:
            raise ValueError(f"trial count differs within family: {durability}/{workload}")
        trial_count = next(iter(trial_counts))
        read_only = workload in set(thresholds["read_only_workloads"])
        min_per_trial = (
            float(thresholds["read_only_min_seconds"])
            if read_only
            else float(thresholds["stateful_min_total_seconds"]) / trial_count
        )
        target_per_trial = (
            float(thresholds["read_only_target_seconds"])
            if read_only
            else float(thresholds["stateful_target_total_seconds"]) / trial_count
        )
        raw_min_ops = fastest_rate * min_per_trial * hard_min_safety_factor
        raw_target_ops = fastest_rate * target_per_trial
        raw_ceiling_ops = slowest_rate * slowest_ceiling_seconds
        quantum_basis = max(raw_min_ops, min(raw_target_ops, raw_ceiling_ops))
        quantum = 100 if quantum_basis < 100_000 else 1000
        min_ops = math.ceil(raw_min_ops / quantum) * quantum
        target_ops = math.ceil(raw_target_ops / quantum) * quantum
        ceiling_ops = math.floor(raw_ceiling_ops / quantum) * quantum
        ceiling_feasible = ceiling_ops >= min_ops
        new_ops = max(old_ops + quantum, min(target_ops, ceiling_ops) if ceiling_feasible else min_ops)
        new_ops_by_family[(durability, workload)] = new_ops
        common[f"{durability}/{workload}"] = new_ops
        refined_families[f"{durability}/{workload}"] = {
            "old_effective_ops": old_ops,
            "new_effective_ops": new_ops,
            "fastest_observed_ops_per_s": fastest_rate,
            "slowest_observed_ops_per_s": slowest_rate,
            "hard_min_seconds_per_trial": min_per_trial,
            "target_seconds_per_trial": target_per_trial,
            "slowest_ceiling_seconds": slowest_ceiling_seconds,
            "estimated_fastest_seconds": new_ops / fastest_rate,
            "estimated_slowest_seconds": new_ops / slowest_rate,
            "slowest_ceiling_feasible": ceiling_feasible,
            "reason": "hard-sizing-threshold",
        }

    refined_groups: list[dict[str, Any]] = []
    for group in groups:
        item = dict(group)
        family = (str(item["durability"]), str(item["workload"]))
        if family in new_ops_by_family:
            old_ops = int(item["suggested_effective_ops"])
            new_ops = new_ops_by_family[family]
            item["refined_from_effective_ops"] = old_ops
            item["suggested_effective_ops"] = new_ops
            item["runner_ops_override"] = new_ops * 2 if item["workload"] == "tiny-txn" else new_ops
            item["source"] = "final-v3-observed-rates-for-refinement"
        refined_groups.append(item)

    out = dict(base_plan)
    out["strategy"] = next_strategy
    out["parent_plan_sha256"] = base_plan_sha256
    out["source_confirmation_results_sha256"] = result_hashes
    out["refined_families"] = refined_families
    out["common_effective_ops"] = common
    out["groups"] = refined_groups
    refinement_policy = {
        "trigger": "hard-sizing-threshold",
        "common_ops_per_durability_workload": True,
        "hard_minimum": "read_only_min_seconds or stateful_min_total_seconds",
        "target": "read_only_target_seconds or stateful_target_total_seconds",
        "slowest_ceiling_seconds": slowest_ceiling_seconds,
        "rounding": "ceil minimum/target and floor ceiling to 100-or-1000-op quanta; hard minimum wins if infeasible",
    }
    if hard_min_safety_factor > 1.0:
        refinement_policy["hard_min_safety_factor"] = hard_min_safety_factor
    out["refinement_policy"] = refinement_policy
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description="Refine completed record final-confirmation families that remain undersized")
    parser.add_argument("base_plan", type=Path)
    parser.add_argument("--run-root", type=Path, required=True)
    parser.add_argument("--thresholds-from-audit", type=Path, required=True)
    parser.add_argument("--run-prefix", required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    base = json.loads(args.base_plan.read_text())
    audit = json.loads(args.thresholds_from_audit.read_text())
    out = refine_plan(
        base,
        base_plan_sha256=sha256(args.base_plan),
        run_root=args.run_root,
        thresholds=audit["thresholds"],
        run_prefix=args.run_prefix,
    )
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")
    print(json.dumps({
        "out": str(args.out),
        "refined_family_count": len(out["refined_families"]),
        "refined_families": sorted(out["refined_families"]),
    }, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
