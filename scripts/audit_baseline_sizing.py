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


def _group_key(row: dict[str, Any]) -> tuple[Any, ...]:
    return tuple(row.get(field) for field in GROUP_FIELDS)


def _cv(values: list[float]) -> float:
    mean = statistics.fmean(values)
    if len(values) < 2 or mean == 0:
        return 0.0
    return statistics.stdev(values) / mean


def audit_rows(
    rows: list[dict[str, Any]],
    *,
    expect_trials: int,
    min_seconds: float,
    target_seconds: float,
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
        median_rate = statistics.median(rates)
        throughput_cv = _cv(rates)
        relative_spread = (max(rates) - min(rates)) / median_rate if median_rate else math.inf

        undersized = median_elapsed < min_seconds
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
        if undersized:
            scaled = effective_ops * target_seconds / max(median_elapsed, 1e-9)
            suggested_effective_ops = _round_up_ops(scaled)
            # The baseline runner intentionally applies the one-key tiny-txn
            # divisor to its profile-wide override. Expose the corresponding
            # runner input as well as the authoritative effective op count.
            runner_ops_override = (
                suggested_effective_ops * 10
                if first.get("workload") == "tiny-txn"
                else suggested_effective_ops
            )

        groups.append(
            {
                "engine": first.get("engine"),
                "engine_version": first.get("engine_version"),
                "durability": first.get("durability"),
                "workload": first.get("workload"),
                "records": first.get("records"),
                "ops_requested": effective_ops,
                "trials": trials,
                "median_elapsed_s": median_elapsed,
                "min_elapsed_s": min(elapsed),
                "max_elapsed_s": max(elapsed),
                "median_ops_per_s": median_rate,
                "throughput_cv": throughput_cv,
                "throughput_relative_spread": relative_spread,
                "status": status,
                "suggested_effective_ops": suggested_effective_ops,
                "runner_ops_override": runner_ops_override,
            }
        )

    counts = {status: sum(group["status"] == status for group in groups) for status in ("accepted", "undersized", "variable")}
    return {
        "row_count": len(rows),
        "group_count": len(groups),
        "thresholds": {
            "expect_trials": expect_trials,
            "min_seconds": min_seconds,
            "target_seconds": target_seconds,
            "max_cv": max_cv,
            "max_relative_spread": max_relative_spread,
        },
        "counts": counts,
        "problems": problems,
        "groups": groups,
    }


def _render_markdown(report: dict[str, Any]) -> str:
    counts = report["counts"]
    thresholds = report["thresholds"]
    lines = [
        "# Baseline sizing audit",
        "",
        f"Rows: **{report['row_count']}**; groups: **{report['group_count']}**; "
        f"accepted: **{counts['accepted']}**; undersized: **{counts['undersized']}**; "
        f"variable: **{counts['variable']}**.",
        "",
        (
            f"Undersized means median measured interval < {thresholds['min_seconds']:.2f} s. "
            f"Suggested budgets target {thresholds['target_seconds']:.2f} s. Long groups are only "
            f"flagged variable when throughput CV > {thresholds['max_cv']:.1%} or relative spread > "
            f"{thresholds['max_relative_spread']:.1%}; increasing their operation count is not proposed."
        ),
        "",
        "| status | engine | durability | workload | ops | median s | CV | spread | suggested effective ops | runner ops override |",
        "| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    order = {"undersized": 0, "variable": 1, "accepted": 2}
    for group in sorted(
        report["groups"],
        key=lambda g: (order[g["status"]], g["median_elapsed_s"], g["engine"], g["durability"], g["workload"]),
    ):
        suggested = group["suggested_effective_ops"]
        runner_override = group["runner_ops_override"]
        lines.append(
            "| {status} | {engine} | {durability} | {workload} | {ops_requested:,} | {median_elapsed_s:.3f} | "
            "{throughput_cv:.1%} | {throughput_relative_spread:.1%} | {suggested} | {runner_override} |".format(
                **group,
                suggested=f"{suggested:,}" if suggested is not None else "—",
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
    parser.add_argument("--min-seconds", type=float, default=2.0)
    parser.add_argument("--target-seconds", type=float, default=3.0)
    parser.add_argument("--max-cv", type=float, default=0.10)
    parser.add_argument("--max-relative-spread", type=float, default=0.25)
    parser.add_argument("--json-out", type=Path)
    parser.add_argument("--markdown-out", type=Path)
    args = parser.parse_args()

    rows = [json.loads(line) for line in args.results.read_text().splitlines() if line.strip()]
    report = audit_rows(
        rows,
        expect_trials=args.expect_trials,
        min_seconds=args.min_seconds,
        target_seconds=args.target_seconds,
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
