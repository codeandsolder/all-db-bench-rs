# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
import math
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any

SIZING_POLICY_VERSION = 2
READ_ONLY_WORKLOADS = frozenset({"point-read", "range-scan", "indexed-read"})
GROUP_FIELDS = (
    "lane",
    "scenario",
    "engine",
    "engine_version",
    "durability",
    "workload",
    "records",
    "ops_requested",
    "clients",
    "value_bytes",
    "value_pattern",
    "key_bytes",
    "key_shape",
    "access_pattern",
    "miss_percent",
    "write_pattern",
    "txn_size",
    "scan_len",
    "settle_ms",
)


def _round_up_ops(value: float) -> int:
    requested = max(1, math.ceil(value))
    if requested < 1_000:
        quantum = 10
    elif requested < 10_000:
        quantum = 100
    elif requested < 100_000:
        quantum = 1_000
    elif requested < 1_000_000:
        quantum = 10_000
    else:
        quantum = 100_000
    return ((requested + quantum - 1) // quantum) * quantum


def _round_up_trials(value: float, *, maximum: int) -> int:
    if maximum < 3:
        raise ValueError("stateful maximum trials must be at least 3")
    requested = max(3, math.ceil(value))
    if requested % 2 == 0:
        requested += 1
    cap = maximum if maximum % 2 == 1 else maximum - 1
    return min(requested, cap)


def _group_key(row: dict[str, Any]) -> tuple[Any, ...]:
    return tuple((row.get(field) if field != "lane" else row.get("lane", "kv")) for field in GROUP_FIELDS)


def _runner_ops_override(row: dict[str, Any], effective_ops: int) -> int:
    if row.get("workload") != "tiny-txn":
        return effective_ops
    lane = row.get("lane", "kv")
    if lane == "kv":
        return effective_ops * 10
    if lane == "record":
        return effective_ops * 2
    raise ValueError(f"unknown tiny-txn baseline lane: {lane!r}")


def _cv(values: list[float]) -> float:
    mean = statistics.fmean(values)
    if len(values) < 2 or mean == 0:
        return 0.0
    return statistics.stdev(values) / mean


def audit_rows(
    rows: list[dict[str, Any]],
    *,
    expect_trials: int,
    read_only_min_seconds: float,
    read_only_target_seconds: float,
    stateful_min_total_seconds: float,
    stateful_target_total_seconds: float,
    stateful_max_trials: int,
    max_cv: float,
    max_relative_spread: float,
) -> dict[str, Any]:
    grouped: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        grouped[_group_key(row)].append(row)

    groups: list[dict[str, Any]] = []
    problems: list[str] = []
    for key, group_rows in sorted(grouped.items(), key=lambda item: tuple(str(v) for v in item[0])):
        first = group_rows[0]
        trials = sorted(int(row["trial"]) for row in group_rows)
        expected = list(range(1, expect_trials + 1))
        if trials != expected:
            problems.append(
                f"{first.get('engine')}/{first.get('durability')}/{first.get('workload')}: "
                f"trials={trials}, expected={expected}"
            )

        elapsed = [float(row["elapsed_s"]) for row in group_rows]
        rates = [float(row["ops_per_s"]) for row in group_rows]
        median_elapsed = statistics.median(elapsed)
        measured_total = sum(elapsed)
        median_rate = statistics.median(rates)
        throughput_cv = _cv(rates)
        relative_spread = (max(rates) - min(rates)) / median_rate if median_rate else math.inf
        workload = str(first.get("workload"))
        read_only = workload in READ_ONLY_WORKLOADS

        undersized = (
            median_elapsed < read_only_min_seconds
            if read_only
            else measured_total < stateful_min_total_seconds
        )
        variable = throughput_cv > max_cv or relative_spread > max_relative_spread
        if undersized:
            status = "undersized"
        elif variable:
            status = "variable"
        else:
            status = "accepted"

        effective_ops = int(first["ops_requested"])
        suggested_effective_ops = None
        runner_ops_override = None
        suggested_trials = None
        resize_strategy = None
        if undersized and read_only:
            scaled = effective_ops * read_only_target_seconds / max(median_elapsed, 1e-9)
            suggested_effective_ops = _round_up_ops(scaled)
            runner_ops_override = _runner_ops_override(first, suggested_effective_ops)
            suggested_trials = expect_trials
            resize_strategy = "more-ops"
        elif undersized:
            # Mutating workloads must preserve the original per-trial state
            # trajectory. Increasing ops would change keyspace growth,
            # tombstone/compaction history, and therefore benchmark semantics.
            suggested_effective_ops = effective_ops
            runner_ops_override = _runner_ops_override(first, effective_ops)
            suggested_trials = _round_up_trials(
                stateful_target_total_seconds / max(median_elapsed, 1e-9),
                maximum=stateful_max_trials,
            )
            resize_strategy = "more-trials"

        groups.append(
            {
                "engine": first.get("engine"),
                "engine_version": first.get("engine_version"),
                "durability": first.get("durability"),
                "workload": workload,
                "records": first.get("records"),
                "ops_requested": effective_ops,
                "trials": trials,
                "median_elapsed_s": median_elapsed,
                "measured_total_s": measured_total,
                "min_elapsed_s": min(elapsed),
                "max_elapsed_s": max(elapsed),
                "median_ops_per_s": median_rate,
                "throughput_cv": throughput_cv,
                "throughput_relative_spread": relative_spread,
                "read_only": read_only,
                "status": status,
                "resize_strategy": resize_strategy,
                "suggested_effective_ops": suggested_effective_ops,
                "runner_ops_override": runner_ops_override,
                "suggested_trials": suggested_trials,
            }
        )

    counts = {
        status: sum(group["status"] == status for group in groups)
        for status in ("accepted", "undersized", "variable")
    }
    strategy_counts = {
        strategy: sum(group.get("resize_strategy") == strategy for group in groups)
        for strategy in ("more-ops", "more-trials")
    }
    return {
        "sizing_policy_version": SIZING_POLICY_VERSION,
        "row_count": len(rows),
        "group_count": len(groups),
        "thresholds": {
            "expect_trials": expect_trials,
            "read_only_workloads": sorted(READ_ONLY_WORKLOADS),
            "read_only_min_seconds": read_only_min_seconds,
            "read_only_target_seconds": read_only_target_seconds,
            "stateful_min_total_seconds": stateful_min_total_seconds,
            "stateful_target_total_seconds": stateful_target_total_seconds,
            "stateful_max_trials": stateful_max_trials,
            "max_cv": max_cv,
            "max_relative_spread": max_relative_spread,
        },
        "counts": counts,
        "strategy_counts": strategy_counts,
        "problems": problems,
        "groups": groups,
    }


def _render_markdown(report: dict[str, Any]) -> str:
    counts = report["counts"]
    strategies = report["strategy_counts"]
    thresholds = report["thresholds"]
    lines = [
        "# Baseline sizing audit",
        "",
        f"Policy v{report['sizing_policy_version']}. Rows: **{report['row_count']}**; "
        f"groups: **{report['group_count']}**; accepted: **{counts['accepted']}**; "
        f"undersized: **{counts['undersized']}**; variable: **{counts['variable']}**.",
        "",
        (
            f"Read-only workloads ({', '.join(thresholds['read_only_workloads'])}) are undersized when "
            f"their median trial is < {thresholds['read_only_min_seconds']:.2f} s and resize ops toward "
            f"{thresholds['read_only_target_seconds']:.2f} s. Stateful workloads preserve the original "
            f"per-trial op count and are undersized only when their existing trials total < "
            f"{thresholds['stateful_min_total_seconds']:.2f} s; they add fresh-database trials toward "
            f"{thresholds['stateful_target_total_seconds']:.2f} s, capped at "
            f"{thresholds['stateful_max_trials']} trials. Planned resizes: "
            f"{strategies['more-ops']} more-ops, {strategies['more-trials']} more-trials."
        ),
        (
            f"Groups not requiring more sampling are flagged variable when throughput CV > "
            f"{thresholds['max_cv']:.1%} or relative spread > {thresholds['max_relative_spread']:.1%}; "
            "operation count is never increased merely to hide real variability."
        ),
        "",
        "| status | strategy | engine | durability | workload | ops | trials | measured s | median s | CV | spread | suggested ops | suggested trials | runner ops override |",
        "| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    order = {"undersized": 0, "variable": 1, "accepted": 2}
    for group in sorted(
        report["groups"],
        key=lambda g: (order[g["status"]], g["median_elapsed_s"], g["engine"], g["durability"], g["workload"]),
    ):
        suggested = group["suggested_effective_ops"]
        runner_override = group["runner_ops_override"]
        suggested_trials = group["suggested_trials"]
        lines.append(
            "| {status} | {strategy} | {engine} | {durability} | {workload} | {ops_requested:,} | {trial_count} | "
            "{measured_total_s:.3f} | {median_elapsed_s:.3f} | {throughput_cv:.1%} | "
            "{throughput_relative_spread:.1%} | {suggested} | {suggested_trials_text} | {runner_override} |".format(
                **group,
                strategy=group["resize_strategy"] or "—",
                trial_count=len(group["trials"]),
                suggested=f"{suggested:,}" if suggested is not None else "—",
                suggested_trials_text=str(suggested_trials) if suggested_trials is not None else "—",
                runner_override=f"{runner_override:,}" if runner_override is not None else "—",
            )
        )
    if report["problems"]:
        lines.extend(["", "## Problems", "", *[f"- {problem}" for problem in report["problems"]]])
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description="Audit baseline benchmark timing stability and sizing")
    parser.add_argument("results", type=Path)
    parser.add_argument("--expect-trials", type=int, default=3)
    parser.add_argument("--read-only-min-seconds", type=float, default=2.0)
    parser.add_argument("--read-only-target-seconds", type=float, default=3.0)
    parser.add_argument("--stateful-min-total-seconds", type=float, default=0.75)
    parser.add_argument("--stateful-target-total-seconds", type=float, default=1.0)
    parser.add_argument("--stateful-max-trials", type=int, default=51)
    parser.add_argument("--max-cv", type=float, default=0.10)
    parser.add_argument("--max-relative-spread", type=float, default=0.25)
    parser.add_argument("--json-out", type=Path)
    parser.add_argument("--markdown-out", type=Path)
    args = parser.parse_args()

    rows = [json.loads(line) for line in args.results.read_text().splitlines() if line.strip()]
    report = audit_rows(
        rows,
        expect_trials=args.expect_trials,
        read_only_min_seconds=args.read_only_min_seconds,
        read_only_target_seconds=args.read_only_target_seconds,
        stateful_min_total_seconds=args.stateful_min_total_seconds,
        stateful_target_total_seconds=args.stateful_target_total_seconds,
        stateful_max_trials=args.stateful_max_trials,
        max_cv=args.max_cv,
        max_relative_spread=args.max_relative_spread,
    )
    if args.json_out:
        args.json_out.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    markdown = _render_markdown(report)
    if args.markdown_out:
        args.markdown_out.write_text(markdown)
    else:
        print(markdown, end="")
    return 1 if report["problems"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
