#!/usr/bin/env -S uv run --script
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


def median(values: list[float]) -> float:
    return statistics.median(values) if values else math.nan


def fmt(value: float, digits: int = 2) -> str:
    if math.isnan(value):
        return "—"
    if abs(value) >= 1000:
        return f"{value:,.0f}"
    return f"{value:.{digits}f}"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("--expect-trials", type=int)
    parser.add_argument("--json-out", type=Path)
    parser.add_argument("--markdown-out", type=Path)
    args = parser.parse_args()

    cases = []
    for path in sorted((args.run_dir / "cases").glob("*.json")):
        cases.append(json.loads(path.read_text()))
    if not cases:
        raise SystemExit("no TSDB case results")

    grouped: dict[str, list[dict[str, object]]] = defaultdict(list)
    for case in cases:
        if case.get("lane") != "tsdb-server":
            raise SystemExit(f"unexpected lane in result: {case.get('lane')}")
        grouped[str(case["engine"])].append(case)

    problems: list[str] = []
    rows = []
    for engine, items in sorted(grouped.items()):
        if args.expect_trials is not None and len(items) != args.expect_trials:
            problems.append(f"{engine}: expected {args.expect_trials} trials, got {len(items)}")
        sample_count = [float(item["total_samples"]) for item in items]
        ingest_rate = [float(item["ingest"]["samples_per_s"]) for item in items]
        startup = [float(item["server_startup_s"]) for item in items]
        server_cpu_per_sample = [
            float(item["server_process"]["cpu_runtime_ns"]) / samples
            for item, samples in zip(items, sample_count, strict=True)
        ]
        server_write_per_sample = [
            float(item["server_process"]["write_bytes"]) / samples
            for item, samples in zip(items, sample_count, strict=True)
        ]
        disk_per_sample = [
            float(item["server_data_bytes"]) / samples
            for item, samples in zip(items, sample_count, strict=True)
        ]
        point_p99 = [float(item["queries"]["point"]["latency"]["p99_us"]) for item in items]
        range_p99 = [float(item["queries"]["range"]["latency"]["p99_us"]) for item in items]
        aggregate_p99 = [float(item["queries"]["aggregate"]["latency"]["p99_us"]) for item in items]
        rows.append(
            {
                "engine": engine,
                "version": items[0]["engine_version"],
                "transport": items[0]["transport"],
                "trials": len(items),
                "median_ingest_samples_per_s": median(ingest_rate),
                "median_server_cpu_ns_per_sample": median(server_cpu_per_sample),
                "median_server_write_bytes_per_sample": median(server_write_per_sample),
                "median_data_bytes_per_sample": median(disk_per_sample),
                "median_startup_s": median(startup),
                "median_point_p99_us": median(point_p99),
                "median_range_p99_us": median(range_p99),
                "median_aggregate_p99_us": median(aggregate_p99),
            }
        )

    summary = {
        "lane": "tsdb-server",
        "cases": len(cases),
        "engines": len(rows),
        "problems": problems,
        "rows": rows,
    }
    json_text = json.dumps(summary, indent=2, sort_keys=True) + "\n"
    if args.json_out:
        args.json_out.write_text(json_text)
    else:
        print(json_text, end="")

    lines = [
        "# TSDB server benchmark summary",
        "",
        "Do not merge this lane with embedded KV or record-product leaderboards. Ingestion transport is explicit because InfluxDB 3 uses native line protocol while the other three use Prometheus Remote Write v1.",
        "",
        "| Engine | Trials | Ingest samples/s | Server CPU ns/sample | Server write B/sample | Data B/sample | Startup s | Point p99 µs | Range p99 µs | Aggregate p99 µs |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for row in rows:
        lines.append(
            "| {engine} {version} | {trials} | {ingest} | {cpu} | {write} | {disk} | {startup} | {point} | {range} | {aggregate} |".format(
                engine=row["engine"],
                version=row["version"],
                trials=row["trials"],
                ingest=fmt(row["median_ingest_samples_per_s"]),
                cpu=fmt(row["median_server_cpu_ns_per_sample"]),
                write=fmt(row["median_server_write_bytes_per_sample"]),
                disk=fmt(row["median_data_bytes_per_sample"]),
                startup=fmt(row["median_startup_s"], 3),
                point=fmt(row["median_point_p99_us"]),
                range=fmt(row["median_range_p99_us"]),
                aggregate=fmt(row["median_aggregate_p99_us"]),
            )
        )
    if problems:
        lines.extend(["", "## Validation problems", "", *[f"- {problem}" for problem in problems]])
    markdown = "\n".join(lines) + "\n"
    if args.markdown_out:
        args.markdown_out.write_text(markdown)
    else:
        print(markdown)
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
