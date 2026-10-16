#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
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


def family_key(group: dict[str, Any]) -> tuple[str, str]:
    return str(group["durability"]), str(group["workload"])


def family_name(family: tuple[str, str]) -> str:
    return f"{family[0]}/{family[1]}"


def _quantum(raw_ops: float) -> int:
    return 100 if raw_ops < 100_000 else 1000


def _load_historical_family(
    family_groups: list[dict[str, Any]],
    *,
    run_root: Path,
    run_prefix: str,
) -> tuple[list[tuple[str, float]], dict[str, str]]:
    engine_rates: list[tuple[str, float]] = []
    result_hashes: dict[str, str] = {}
    for group in family_groups:
        rid = run_id(group, run_prefix)
        result_path = run_root / rid / "results.ndjson"
        summary_path = run_root / rid / "summary.json"
        if not result_path.is_file() or not summary_path.is_file():
            raise ValueError(f"missing historical confirmation result: {rid}")
        expected_trials = int(group["suggested_trials"])
        summary = json.loads(summary_path.read_text())
        if (
            int(summary.get("row_count", -1)) != expected_trials
            or int(summary.get("group_count", -1)) != 1
            or summary.get("problems")
        ):
            raise ValueError(f"invalid historical confirmation summary: {rid}")
        rows = read_ndjson(result_path)
        if sorted(int(row["trial"]) for row in rows) != list(range(1, expected_trials + 1)):
            raise ValueError(f"historical confirmation trial mismatch: {rid}")
        expected_ops = int(group["suggested_effective_ops"])
        if {int(row["ops_requested"]) for row in rows} != {expected_ops}:
            raise ValueError(f"historical confirmation op mismatch: {rid}")
        if {
            (str(row["engine"]), str(row["durability"]), str(row["workload"]))
            for row in rows
        } != {
            (
                str(group["engine"]),
                str(group["durability"]),
                str(group["workload"]),
            )
        }:
            raise ValueError(f"historical confirmation identity mismatch: {rid}")
        engine_rates.append(
            (
                str(group["engine"]),
                statistics.median(float(row["ops_per_s"]) for row in rows),
            )
        )
        result_hashes[rid] = sha256(result_path)
    return engine_rates, result_hashes


def make_safety_plan(
    parent: dict[str, Any],
    *,
    parent_sha256: str,
    run_root: Path,
    run_prefix: str,
    target_fastest_seconds: float,
) -> dict[str, Any]:
    if parent.get("quality_policy_version") != 1:
        raise ValueError("unsupported parent quality policy")
    groups = [dict(group) for group in parent.get("groups", [])]
    if not groups:
        raise ValueError("parent plan has no groups")
    if target_fastest_seconds <= 0:
        raise ValueError("target_fastest_seconds must be positive")

    duration_bounds = parent.get("estimated_duration_bounds")
    if not isinstance(duration_bounds, dict):
        raise ValueError("parent plan is missing estimated_duration_bounds")

    by_family: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for group in groups:
        by_family.setdefault(family_key(group), []).append(group)

    candidate_names = {
        name
        for name, bounds in duration_bounds.items()
        if isinstance(bounds, dict)
        and float(bounds.get("estimated_fastest_s", float("inf"))) < target_fastest_seconds
    }
    unknown = candidate_names - {family_name(family) for family in by_family}
    if unknown:
        raise ValueError(f"duration bounds reference unknown families: {sorted(unknown)}")

    common = dict(parent.get("common_effective_ops", {}))
    updated_bounds = dict(duration_bounds)
    source_hashes: dict[str, str] = {}
    safety_families: dict[str, dict[str, Any]] = {}
    new_ops_by_family: dict[tuple[str, str], int] = {}

    for family in sorted(by_family):
        name = family_name(family)
        if name not in candidate_names:
            continue
        family_groups = by_family[family]
        engines = {str(group["engine"]) for group in family_groups}
        if len(family_groups) != 4 or len(engines) != 4:
            raise ValueError(f"candidate family is not exactly four engines: {name}")
        ops_values = {int(group["suggested_effective_ops"]) for group in family_groups}
        trial_values = {int(group["suggested_trials"]) for group in family_groups}
        records_values = {int(group["records"]) for group in family_groups}
        if len(ops_values) != 1 or len(trial_values) != 1 or len(records_values) != 1:
            raise ValueError(f"candidate family is not common-work/common-shape: {name}")
        old_ops = next(iter(ops_values))
        engine_rates, hashes = _load_historical_family(
            family_groups,
            run_root=run_root,
            run_prefix=run_prefix,
        )
        source_hashes.update(hashes)
        fastest_engine, fastest_rate = max(engine_rates, key=lambda item: item[1])
        slowest_engine, slowest_rate = min(engine_rates, key=lambda item: item[1])
        raw_ops = fastest_rate * target_fastest_seconds
        quantum = _quantum(raw_ops)
        new_ops = max(old_ops, math.ceil(raw_ops / quantum) * quantum)
        if new_ops <= old_ops:
            raise ValueError(
                f"candidate family did not gain headroom: {name}: old={old_ops} new={new_ops}"
            )
        new_ops_by_family[family] = new_ops
        common[name] = new_ops
        updated_bounds[name] = {
            "estimated_fastest_s": new_ops / fastest_rate,
            "estimated_slowest_s": new_ops / slowest_rate,
            "fastest_engine": fastest_engine,
            "slowest_engine": slowest_engine,
        }
        safety_families[name] = {
            "old_effective_ops": old_ops,
            "new_effective_ops": new_ops,
            "historical_fastest_engine": fastest_engine,
            "historical_fastest_ops_per_s": fastest_rate,
            "historical_slowest_engine": slowest_engine,
            "historical_slowest_ops_per_s": slowest_rate,
            "target_fastest_seconds": target_fastest_seconds,
            "estimated_fastest_seconds": new_ops / fastest_rate,
            "estimated_slowest_seconds": new_ops / slowest_rate,
            "quantum": quantum,
            "reason": "continuous-admission-preflight-headroom",
        }

    if not safety_families:
        raise ValueError("no family requires continuous-admission headroom")

    updated_groups: list[dict[str, Any]] = []
    for group in groups:
        item = dict(group)
        family = family_key(item)
        if family in new_ops_by_family:
            old_ops = int(item["suggested_effective_ops"])
            new_ops = new_ops_by_family[family]
            item["continuous_safety_from_effective_ops"] = old_ops
            item["suggested_effective_ops"] = new_ops
            item["runner_ops_override"] = new_ops * 2 if item["workload"] == "tiny-txn" else new_ops
            item["source"] = "historical-final-v3-rates-for-continuous-safety"
        updated_groups.append(item)

    out = dict(parent)
    out["continuous_safety_plan_version"] = 1
    out["continuous_safety_parent_plan_sha256"] = parent_sha256
    out["continuous_safety_policy"] = {
        "candidate_rule": "parent estimated_fastest_s < target_fastest_seconds",
        "target_fastest_seconds": target_fastest_seconds,
        "historical_rate_statistic": "median ops_per_s over complete five-trial v2 family",
        "rounding": "ceil to 100 ops below 100k, otherwise 1000 ops",
        "common_ops_per_durability_workload": True,
        "publication_admission_policy": "pre-io+pre/continuous/post-external-v3",
    }
    out["continuous_safety_source_results_sha256"] = dict(sorted(source_hashes.items()))
    out["continuous_safety_families"] = safety_families
    out["common_effective_ops"] = common
    out["estimated_duration_bounds"] = updated_bounds
    out["groups"] = updated_groups
    return out


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Add deterministic pre-run duration headroom to the continuous record confirmation"
    )
    parser.add_argument("parent_plan", type=Path)
    parser.add_argument("--run-root", type=Path, required=True)
    parser.add_argument("--run-prefix", required=True)
    parser.add_argument("--target-fastest-seconds", type=float, default=2.2)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    parent = json.loads(args.parent_plan.read_text())
    out = make_safety_plan(
        parent,
        parent_sha256=sha256(args.parent_plan),
        run_root=args.run_root,
        run_prefix=args.run_prefix,
        target_fastest_seconds=args.target_fastest_seconds,
    )
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")
    print(
        json.dumps(
            {
                "out": str(args.out),
                "family_count": len(out["continuous_safety_families"]),
                "families": out["continuous_safety_families"],
                "plan_sha256": sha256(args.out),
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
