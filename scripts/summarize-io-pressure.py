#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///

from __future__ import annotations

import argparse
import json
import re
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any

SCENARIO_RE = re.compile(r"^io-pressure-(\d+)pct$")


def number(value: Any) -> float:
    try:
        return float(value or 0)
    except (TypeError, ValueError):
        return 0.0


def median(values: list[float]) -> float | None:
    return statistics.median(values) if values else None


def ratio(value: float | None, baseline: float | None) -> float | None:
    if value is None or baseline is None or baseline <= 0:
        return None
    return value / baseline


def p99_us(row: dict[str, Any], field: str) -> float | None:
    q = row.get(field) or {}
    value = q.get("p99_us")
    return number(value) if value is not None else None


def cpu_ns_per_op(row: dict[str, Any]) -> float | None:
    ops = int(number(row.get("ops_completed")))
    if ops <= 0:
        return None
    proc = row.get("measured_process") or {}
    return number(proc.get("cpu_runtime_ns")) / ops


def process_io_bytes_per_op(row: dict[str, Any]) -> float | None:
    ops = int(number(row.get("ops_completed")))
    if ops <= 0:
        return None
    proc = row.get("measured_process") or {}
    return (number(proc.get("read_bytes")) + number(proc.get("write_bytes"))) / ops


def load_rows(run_dir: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for path in sorted((run_dir / "cases").glob("*.json")):
        row = json.loads(path.read_text())
        match = SCENARIO_RE.match(str(row.get("scenario", "")))
        if not match:
            continue
        pct = int(match.group(1))
        meta_path = run_dir / "pressure-meta" / path.name
        if not meta_path.is_file():
            raise FileNotFoundError(f"missing pressure metadata for {path.name}: {meta_path}")
        pressure = json.loads(meta_path.read_text())
        rows.append(
            {
                "case_id": path.stem,
                "engine": row.get("engine"),
                "engine_version": row.get("engine_version"),
                "durability": row.get("durability"),
                "durability_mapping": row.get("durability_mapping"),
                "workload": row.get("workload"),
                "trial": row.get("trial"),
                "pressure_percent": pct,
                "ops_per_s": number(row.get("ops_per_s")),
                "read_p99_us": p99_us(row, "read_latency"),
                "write_p99_us": p99_us(row, "write_txn_latency"),
                "cpu_ns_per_op": cpu_ns_per_op(row),
                "process_io_bytes_per_op": process_io_bytes_per_op(row),
                "target_pressure_iops": number(pressure.get("target_iops")),
                "delivered_pressure_iops": number(pressure.get("delivered_iops")),
                "delivered_vs_target": (
                    number(pressure.get("delivered_iops")) / number(pressure.get("target_iops"))
                    if number(pressure.get("target_iops")) > 0
                    else None
                ),
            }
        )
    return rows


def aggregate(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    grouped: dict[tuple[str, str, str, int], list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        key = (
            str(row["engine"]),
            str(row["durability"]),
            str(row["workload"]),
            int(row["pressure_percent"]),
        )
        grouped[key].append(row)

    raw: list[dict[str, Any]] = []
    for (engine, durability, workload, pct), items in grouped.items():
        def vals(name: str) -> list[float]:
            return [float(x[name]) for x in items if x.get(name) is not None]

        raw.append(
            {
                "engine": engine,
                "durability": durability,
                "workload": workload,
                "pressure_percent": pct,
                "trials": len(items),
                "ops_per_s_median": median(vals("ops_per_s")),
                "read_p99_us_median": median(vals("read_p99_us")),
                "write_p99_us_median": median(vals("write_p99_us")),
                "cpu_ns_per_op_median": median(vals("cpu_ns_per_op")),
                "process_io_bytes_per_op_median": median(vals("process_io_bytes_per_op")),
                "target_pressure_iops_median": median(vals("target_pressure_iops")),
                "delivered_pressure_iops_median": median(vals("delivered_pressure_iops")),
                "delivered_vs_target_median": median(vals("delivered_vs_target")),
            }
        )

    baseline = {
        (x["engine"], x["durability"], x["workload"]): x
        for x in raw
        if x["pressure_percent"] == 0
    }
    for row in raw:
        base = baseline.get((row["engine"], row["durability"], row["workload"]))
        row["throughput_ratio_vs_0pct"] = ratio(
            row["ops_per_s_median"], None if base is None else base["ops_per_s_median"]
        )
        row["read_p99_ratio_vs_0pct"] = ratio(
            row["read_p99_us_median"], None if base is None else base["read_p99_us_median"]
        )
        row["write_p99_ratio_vs_0pct"] = ratio(
            row["write_p99_us_median"], None if base is None else base["write_p99_us_median"]
        )
    return sorted(raw, key=lambda x: (x["engine"], x["workload"], x["pressure_percent"]))


def fmt(value: float | None, digits: int = 2) -> str:
    return "-" if value is None else f"{value:.{digits}f}"


def markdown(rows: list[dict[str, Any]]) -> str:
    out = [
        "# Calibrated I/O-pressure dependence",
        "",
        "Throughput and latency ratios are relative to the same engine/workload at 0% external pressure.",
        "",
        "| engine | workload | pressure | delivered/target | ops/s | throughput vs 0% | read p99 vs 0% | write p99 vs 0% |",
        "|---|---|---:|---:|---:|---:|---:|---:|",
    ]
    for row in rows:
        out.append(
            f"| {row['engine']} | {row['workload']} | {row['pressure_percent']}% | "
            f"{fmt(row['delivered_vs_target_median'])} | {fmt(row['ops_per_s_median'], 1)} | "
            f"{fmt(row['throughput_ratio_vs_0pct'])} | {fmt(row['read_p99_ratio_vs_0pct'])} | "
            f"{fmt(row['write_p99_ratio_vs_0pct'])} |"
        )
    out.append("")
    return "\n".join(out)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("--json-out", type=Path)
    parser.add_argument("--markdown-out", type=Path)
    args = parser.parse_args()

    cases = load_rows(args.run_dir)
    if not cases:
        raise ValueError(f"no io-pressure cases found under {args.run_dir / 'cases'}")
    rows = aggregate(cases)
    summary = {
        "format_version": 1,
        "lane": "io-pressure",
        "successful_cases": len(cases),
        "rows": rows,
    }
    text = json.dumps(summary, indent=2, sort_keys=True) + "\n"
    md = markdown(rows)
    if args.json_out:
        args.json_out.write_text(text)
    else:
        print(text, end="")
    if args.markdown_out:
        args.markdown_out.write_text(md)


if __name__ == "__main__":
    main()
