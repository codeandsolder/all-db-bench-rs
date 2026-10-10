#!/usr/bin/env -S uv run --script
from __future__ import annotations

import argparse
import hashlib
import json
import math
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any

PRODUCTS = ("sqlite", "surrealdb", "surrealdb-rocksdb", "turso")
DURABILITIES = ("relaxed", "sync")
WORKLOADS = ("indexed-read", "point-read", "read-heavy", "tiny-txn", "write-burst")


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def read_rows(path: Path) -> list[dict[str, Any]]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def build_plan(
    rows: list[dict[str, Any]],
    *,
    source_sha256: str,
    trials: int = 5,
    fastest_target_seconds: float = 3.0,
    slowest_ceiling_seconds: float = 15.0,
) -> dict[str, Any]:
    if trials < 3:
        raise ValueError("final confirmation requires at least three trials")
    by_group: dict[tuple[str, str, str], list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        if row.get("read_materialization") != "full-record-v1" or row.get("write_materialization") != "no-return-v1":
            raise ValueError("record final sizing input has wrong materialization semantics")
        key = (str(row["engine"]), str(row["durability"]), str(row["workload"]))
        by_group[key].append(row)
    expected = {(e, d, w) for e in PRODUCTS for d in DURABILITIES for w in WORKLOADS}
    if set(by_group) != expected:
        missing = sorted(expected - set(by_group))
        extra = sorted(set(by_group) - expected)
        raise ValueError(f"record final sizing identity mismatch: missing={missing} extra={extra}")

    rates: dict[tuple[str, str, str], float] = {}
    records_by_family: dict[tuple[str, str], set[int]] = defaultdict(set)
    for key, items in by_group.items():
        rates[key] = statistics.median(float(item["ops_per_s"]) for item in items)
        records = {int(item["records"]) for item in items}
        if len(records) != 1:
            raise ValueError(f"record count changed within group {key}: {sorted(records)}")
        records_by_family[(key[1], key[2])].update(records)

    common: dict[tuple[str, str], int] = {}
    estimates: dict[str, dict[str, Any]] = {}
    for durability in DURABILITIES:
        for workload in WORKLOADS:
            family = [(engine, rates[(engine, durability, workload)]) for engine in PRODUCTS]
            fastest = max(family, key=lambda item: item[1])
            slowest = min(family, key=lambda item: item[1])
            raw_ops = min(fastest[1] * fastest_target_seconds, slowest[1] * slowest_ceiling_seconds)
            quantum = 100 if raw_ops < 100_000 else 1000
            ops = max(100, math.floor(raw_ops / quantum) * quantum)
            common[(durability, workload)] = ops
            estimates[f"{durability}/{workload}"] = {
                "fastest_engine": fastest[0],
                "slowest_engine": slowest[0],
                "estimated_fastest_s": ops / fastest[1],
                "estimated_slowest_s": ops / slowest[1],
            }

    groups: list[dict[str, Any]] = []
    for durability in DURABILITIES:
        for workload in WORKLOADS:
            records = records_by_family[(durability, workload)]
            if len(records) != 1:
                raise ValueError(f"record count differs across engines for {durability}/{workload}: {sorted(records)}")
            records_value = next(iter(records))
            ops = common[(durability, workload)]
            for engine in PRODUCTS:
                groups.append(
                    {
                        "engine": engine,
                        "durability": durability,
                        "workload": workload,
                        "records": records_value,
                        "median_elapsed_s": 3.0,
                        "suggested_effective_ops": ops,
                        "runner_ops_override": ops * 2 if workload == "tiny-txn" else ops,
                        "suggested_trials": trials,
                        "quality_repair_required": True,
                        "resize_strategy": "quality-repair",
                        "source": "selected-final-v2-rates-for-sizing-only",
                    }
                )

    return {
        "quality_policy_version": 1,
        "lane": "record",
        "strategy": "final-five-trial-common-work-v3" if trials == 5 else "final-common-work-v3",
        "source_selection": "selected-final-v2-sizing-only",
        "source_selection_results_sha256": source_sha256,
        "sizing_rule": {
            "fastest_target_seconds": fastest_target_seconds,
            "slowest_ceiling_seconds": slowest_ceiling_seconds,
            "rounding": "floor-to-100-or-1000-ops",
            "common_ops_per_durability_workload": True,
        },
        "common_effective_ops": {f"{d}/{w}": v for (d, w), v in sorted(common.items())},
        "estimated_duration_bounds": estimates,
        "group_count": len(groups),
        "trials_per_group": trials,
        "groups": groups,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="Plan final record confirmation with common work per durability/workload")
    parser.add_argument("results", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--trials", type=int, default=5)
    parser.add_argument("--fastest-target-seconds", type=float, default=3.0)
    parser.add_argument("--slowest-ceiling-seconds", type=float, default=15.0)
    args = parser.parse_args()
    plan = build_plan(
        read_rows(args.results),
        source_sha256=sha256(args.results),
        trials=args.trials,
        fastest_target_seconds=args.fastest_target_seconds,
        slowest_ceiling_seconds=args.slowest_ceiling_seconds,
    )
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(plan, indent=2, sort_keys=True) + "\n")
    print(json.dumps({"out": str(args.out), "group_count": plan["group_count"], "trials": plan["trials_per_group"]}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
