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
from typing import Any, Iterable


def median(xs: Iterable[float]) -> float | None:
    vals = list(xs)
    return statistics.median(vals) if vals else None


def quantile(xs: Iterable[float], q: float) -> float | None:
    vals = sorted(xs)
    if not vals:
        return None
    if len(vals) == 1:
        return vals[0]
    pos = q * (len(vals) - 1)
    lo = math.floor(pos)
    hi = math.ceil(pos)
    if lo == hi:
        return vals[lo]
    frac = pos - lo
    return vals[lo] * (1.0 - frac) + vals[hi] * frac


def ratio(num: float, den: float) -> float | None:
    return num / den if den > 0 else None


def longest_run(flags: list[bool]) -> int:
    best = cur = 0
    for flag in flags:
        if flag:
            cur += 1
            best = max(best, cur)
        else:
            cur = 0
    return best


def frac(delta: dict[str, Any], numerator: str) -> float:
    wall_ns = max(float(delta.get("accounting_wall_ns", 0)), 1.0)
    if numerator.endswith("_us"):
        return float(delta.get(numerator, 0)) * 1000.0 / wall_ns
    return float(delta.get(numerator, 0)) / wall_ns


def summarize_case(r: dict[str, Any]) -> dict[str, Any]:
    windows = r.get("windows", [])
    if not windows:
        raise ValueError("sustained result has no windows")

    rates = [float(w["ops_per_s"]) for w in windows]
    p99 = [float(w["transaction_latency"]["p99_us"]) for w in windows]
    baseline_n = min(3, len(windows))
    baseline_rate = statistics.median(rates[:baseline_n])
    baseline_p99 = statistics.median(p99[:baseline_n])
    rate_ratio = [x / baseline_rate if baseline_rate > 0 else math.nan for x in rates]
    tail_n = min(3, len(rates))
    tail_rate = statistics.median(rates[-tail_n:])
    mean_rate = statistics.mean(rates)
    throughput_cv = statistics.pstdev(rates) / mean_rate if len(rates) > 1 and mean_rate > 0 else 0.0
    if len(rates) > 1 and baseline_rate > 0:
        x_mean = (len(rates) - 1) / 2.0
        y_mean = statistics.mean(rates)
        denom = sum((i - x_mean) ** 2 for i in range(len(rates)))
        slope = sum((i - x_mean) * (rate - y_mean) for i, rate in enumerate(rates)) / denom
        normalized_slope_per_window = slope / baseline_rate
    else:
        normalized_slope_per_window = 0.0
    below75 = [x < 0.75 for x in rate_ratio]
    below50 = [x < 0.50 for x in rate_ratio]
    below25 = [x < 0.25 for x in rate_ratio]

    first50 = next((i for i, x in enumerate(below50) if x), None)
    recovered90 = None
    max_after_first50 = None
    if first50 is not None:
        later = rate_ratio[first50 + 1 :]
        max_after_first50 = max(later) if later else None
        recovered90 = next(
            (first50 + 1 + i for i, x in enumerate(later) if x >= 0.90),
            None,
        )

    cpu_ns_per_op = []
    write_amp = []
    runqueue = []
    cpu_psi = []
    io_psi_full = []
    for w in windows:
        ops = max(int(w.get("ops_completed", 0)), 1)
        logical = max(int(w.get("logical_mutated_bytes", 0)), 1)
        proc = w.get("process", {})
        system = w.get("system", {})
        cpu_ns_per_op.append(float(proc.get("cpu_runtime_ns", 0)) / ops)
        write_amp.append(float(proc.get("write_bytes", 0)) / logical)
        runqueue.append(float(proc.get("runqueue_wait_fraction_of_wall", 0.0)))
        cpu_psi.append(frac(system, "psi_cpu_some_us"))
        io_psi_full.append(frac(system, "psi_io_full_us"))

    settle = r.get("post_workload_settle")
    settle_summary = None
    if settle:
        proc = settle.get("process", {})
        settle_summary = {
            "elapsed_s": float(settle.get("elapsed_s", 0.0)),
            "cpu_runtime_ns": int(proc.get("cpu_runtime_ns", 0)),
            "write_bytes": int(proc.get("write_bytes", 0)),
            "db_bytes_delta": int(settle.get("db_bytes_after", 0))
            - int(settle.get("db_bytes_before", 0)),
            "sample_count": len(settle.get("samples", [])),
        }

    return {
        "format_version": r.get("format_version"),
        "lane": r.get("lane"),
        "scenario": r.get("scenario"),
        "engine": r.get("engine"),
        "engine_version": r.get("engine_version"),
        "durability": r.get("durability"),
        "durability_mapping": r.get("durability_mapping"),
        "pattern": r.get("pattern"),
        "records": r.get("records"),
        "ops_requested": r.get("ops_requested"),
        "ops_completed": r.get("ops_completed"),
        "window_ops": r.get("window_ops"),
        "value_bytes": r.get("value_bytes"),
        "value_pattern": r.get("value_pattern"),
        "key_bytes": r.get("key_bytes"),
        "key_shape": r.get("key_shape"),
        "txn_size": r.get("txn_size"),
        "trial": r.get("trial"),
        "window_count": len(windows),
        "wall_ops_per_s": float(r.get("ops_per_s", 0.0)),
        "active_ops_per_s": float(r.get("active_ops_per_s", 0.0)),
        "instrumentation_fraction": float(r.get("instrumentation_fraction_of_wall", 0.0)),
        "aggregate_cpu_ns_per_op": float(r.get("measured_process", {}).get("cpu_runtime_ns", 0))
        / max(int(r.get("ops_completed", 0)), 1),
        "aggregate_process_write_bytes_per_logical_mutated_byte": float(
            r.get("measured_process", {}).get("write_bytes", 0)
        ) / max(int(r.get("logical_mutated_bytes", 0)), 1),
        "baseline_windows": baseline_n,
        "baseline_ops_per_s": baseline_rate,
        "window_ops_per_s_median": statistics.median(rates),
        "window_ops_per_s_q1": quantile(rates, 0.25),
        "window_ops_per_s_q3": quantile(rates, 0.75),
        "window_ops_per_s_min": min(rates),
        "window_ops_per_s_max": max(rates),
        "tail_windows": tail_n,
        "tail_ops_per_s_median": tail_rate,
        "tail_throughput_ratio_vs_baseline": ratio(tail_rate, baseline_rate),
        "throughput_cv": throughput_cv,
        "normalized_throughput_slope_per_window": normalized_slope_per_window,
        "min_throughput_ratio_vs_baseline": min(rate_ratio),
        "windows_below_75pct_baseline": sum(below75),
        "windows_below_50pct_baseline": sum(below50),
        "windows_below_25pct_baseline": sum(below25),
        "longest_run_below_50pct_baseline": longest_run(below50),
        "first_below_50pct_window": first50,
        "first_below_50pct_ops_start": windows[first50]["ops_start"] if first50 is not None else None,
        "first_recovered_90pct_window": recovered90,
        "max_throughput_ratio_after_first_50pct_cliff": max_after_first50,
        "baseline_p99_txn_us": baseline_p99,
        "worst_p99_txn_us": max(p99),
        "worst_p99_multiplier_vs_baseline": ratio(max(p99), baseline_p99),
        "cpu_ns_per_op_median_window": statistics.median(cpu_ns_per_op),
        "cpu_ns_per_op_max_window": max(cpu_ns_per_op),
        "process_write_bytes_per_logical_mutated_byte_median_window": statistics.median(write_amp),
        "process_write_bytes_per_logical_mutated_byte_max_window": max(write_amp),
        "runqueue_wait_fraction_median_window": statistics.median(runqueue),
        "runqueue_wait_fraction_max_window": max(runqueue),
        "cpu_psi_some_fraction_median_window": statistics.median(cpu_psi),
        "cpu_psi_some_fraction_max_window": max(cpu_psi),
        "io_psi_full_fraction_median_window": statistics.median(io_psi_full),
        "io_psi_full_fraction_max_window": max(io_psi_full),
        "db_bytes_before": int(r.get("db_bytes_before", 0)),
        "db_bytes_after_foreground": int(r.get("db_bytes_after_foreground", 0)),
        "db_bytes_final": int(r.get("db_bytes_final", 0)),
        "foreground_db_growth_bytes": int(r.get("db_bytes_after_foreground", 0))
        - int(r.get("db_bytes_before", 0)),
        "settle": settle_summary,
    }


def group_key(c: dict[str, Any]) -> tuple[Any, ...]:
    return (
        c["format_version"], c["scenario"], c["engine"], c["engine_version"],
        c["durability"], c["durability_mapping"], c["pattern"], c["records"],
        c["ops_requested"], c["window_ops"], c["value_bytes"], c["value_pattern"],
        c["key_bytes"], c["key_shape"], c["txn_size"],
    )


def aggregate(cases: list[dict[str, Any]]) -> list[dict[str, Any]]:
    groups: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
    for c in cases:
        groups[group_key(c)].append(c)
    out = []
    numeric = [
        "wall_ops_per_s", "active_ops_per_s", "instrumentation_fraction",
        "aggregate_cpu_ns_per_op", "aggregate_process_write_bytes_per_logical_mutated_byte",
        "baseline_ops_per_s", "window_ops_per_s_median", "window_ops_per_s_min",
        "tail_ops_per_s_median", "tail_throughput_ratio_vs_baseline", "throughput_cv",
        "normalized_throughput_slope_per_window", "min_throughput_ratio_vs_baseline",
        "windows_below_75pct_baseline",
        "windows_below_50pct_baseline", "windows_below_25pct_baseline",
        "longest_run_below_50pct_baseline", "baseline_p99_txn_us",
        "worst_p99_txn_us", "worst_p99_multiplier_vs_baseline",
        "cpu_ns_per_op_median_window", "cpu_ns_per_op_max_window",
        "process_write_bytes_per_logical_mutated_byte_median_window",
        "process_write_bytes_per_logical_mutated_byte_max_window",
        "runqueue_wait_fraction_median_window", "runqueue_wait_fraction_max_window",
        "cpu_psi_some_fraction_median_window", "cpu_psi_some_fraction_max_window",
        "io_psi_full_fraction_median_window", "io_psi_full_fraction_max_window",
        "foreground_db_growth_bytes",
    ]
    for key, rs in sorted(groups.items(), key=lambda kv: tuple(str(x) for x in kv[0])):
        first = rs[0]
        g = {k: first[k] for k in [
            "format_version", "scenario", "engine", "engine_version", "durability",
            "durability_mapping", "pattern", "records", "ops_requested", "window_ops",
            "value_bytes", "value_pattern", "key_bytes", "key_shape", "txn_size",
        ]}
        g["trials"] = len(rs)
        for field in numeric:
            vals = [float(r[field]) for r in rs if r.get(field) is not None]
            g[field + "_median"] = statistics.median(vals) if vals else None
            if vals:
                g[field + "_min"] = min(vals)
                g[field + "_max"] = max(vals)
        first_cliffs = [r["first_below_50pct_window"] for r in rs if r["first_below_50pct_window"] is not None]
        g["trials_with_50pct_cliff"] = len(first_cliffs)
        g["first_below_50pct_window_median_when_present"] = median(float(x) for x in first_cliffs)
        g["trials_recovered_to_90pct_after_cliff"] = sum(
            r["first_below_50pct_window"] is not None and r["first_recovered_90pct_window"] is not None
            for r in rs
        )
        settle_cpu = [r["settle"]["cpu_runtime_ns"] for r in rs if r.get("settle")]
        settle_write = [r["settle"]["write_bytes"] for r in rs if r.get("settle")]
        settle_db = [r["settle"]["db_bytes_delta"] for r in rs if r.get("settle")]
        g["settle_cpu_runtime_ns_median"] = median(float(x) for x in settle_cpu)
        g["settle_write_bytes_median"] = median(float(x) for x in settle_write)
        g["settle_db_bytes_delta_median"] = median(float(x) for x in settle_db)
        out.append(g)
    return out


def fmt(v: Any, digits: int = 3) -> str:
    if v is None:
        return "—"
    if isinstance(v, float):
        if math.isnan(v) or math.isinf(v):
            return str(v)
        return f"{v:.{digits}f}"
    return str(v)


def main() -> None:
    ap = argparse.ArgumentParser(description="Summarize windowed sustained-write/compaction runs")
    ap.add_argument("input", type=Path, help="run directory, results.ndjson, or one JSON result")
    ap.add_argument("--expect-trials", type=int)
    ap.add_argument("--json-out", type=Path)
    ap.add_argument("--markdown-out", type=Path)
    args = ap.parse_args()

    inp = args.input
    if inp.is_dir():
        paths = sorted((inp / "cases").glob("*.json")) if (inp / "cases").is_dir() else []
        raw = [json.loads(p.read_text()) for p in paths]
    elif inp.name.endswith(".ndjson"):
        raw = [json.loads(line) for line in inp.read_text().splitlines() if line.strip()]
    else:
        raw = [json.loads(inp.read_text())]

    raw = [r for r in raw if r.get("lane") == "kv-sustained"]
    cases = [summarize_case(r) for r in raw]
    groups = aggregate(cases)
    problems: list[str] = []
    warnings: list[str] = []
    for c in cases:
        if c["instrumentation_fraction"] > 0.10:
            warnings.append(
                f"{c['scenario']} {c['engine']} {c['durability']} {c['pattern']} trial={c['trial']}: "
                f"instrumentation is {100.0 * c['instrumentation_fraction']:.1f}% of sustained foreground wall time; "
                "use larger windows before interpreting cliff/performance magnitude"
            )
    if args.expect_trials is not None:
        for g in groups:
            if g["trials"] != args.expect_trials:
                problems.append(
                    f"{g['scenario']} {g['engine']} {g['durability']} {g['pattern']}: "
                    f"{g['trials']} trials, expected {args.expect_trials}"
                )

    result = {
        "case_count": len(cases),
        "group_count": len(groups),
        "problems": problems,
        "warnings": warnings,
        "cases": cases,
        "groups": groups,
    }
    text = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(text)
    else:
        print(text, end="")

    if args.markdown_out:
        lines = [
            "# Sustained-write / compaction summary", "",
            "Threshold columns are diagnostics relative to each trial's first three windows; they are not pass/fail criteria.",
            "Cases with >10% between-window instrumentation overhead are warned in summary.json and need larger windows before performance/cliff magnitude is interpreted.", "",
            "| scenario | engine | dur | pattern | trials | wall ops/s | tail/base | slope/window | CV | min/base | 50% cliff trials | longest <50% | worst p99/base | write B/logical B agg | write B/logical B max-win | rq max % | IO PSI full max % | instr % | settle write MiB |",
            "|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
        ]
        for g in groups:
            settle_mib = None if g["settle_write_bytes_median"] is None else g["settle_write_bytes_median"] / 1024 / 1024
            lines.append(
                "| {scenario} | {engine} {version} | {dur} | {pattern} | {trials} | {ops} | {tailr} | {slope} | {cv} | {minr} | {cliffs} | {longest} | {p99} | {waagg} | {wa} | {rq} | {io} | {instr} | {settle} |".format(
                    scenario=g["scenario"], engine=g["engine"], version=g["engine_version"],
                    dur=g["durability"], pattern=g["pattern"], trials=g["trials"],
                    ops=fmt(g["wall_ops_per_s_median"]),
                    tailr=fmt(g["tail_throughput_ratio_vs_baseline_median"]),
                    slope=fmt(g["normalized_throughput_slope_per_window_median"]),
                    cv=fmt(g["throughput_cv_median"]),
                    minr=fmt(g["min_throughput_ratio_vs_baseline_median"]),
                    cliffs=g["trials_with_50pct_cliff"],
                    longest=fmt(g["longest_run_below_50pct_baseline_median"]),
                    p99=fmt(g["worst_p99_multiplier_vs_baseline_median"]),
                    waagg=fmt(g["aggregate_process_write_bytes_per_logical_mutated_byte_median"]),
                    wa=fmt(g["process_write_bytes_per_logical_mutated_byte_max_window_median"]),
                    rq=fmt(100.0 * g["runqueue_wait_fraction_max_window_median"]),
                    io=fmt(100.0 * g["io_psi_full_fraction_max_window_median"]),
                    instr=fmt(100.0 * g["instrumentation_fraction_median"]),
                    settle=fmt(settle_mib),
                )
            )
        args.markdown_out.parent.mkdir(parents=True, exist_ok=True)
        args.markdown_out.write_text("\n".join(lines) + "\n")

    if problems:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
