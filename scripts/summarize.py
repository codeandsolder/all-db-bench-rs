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


def quantile(xs: list[float], q: float) -> float:
    ys = sorted(xs)
    if not ys:
        return math.nan
    if len(ys) == 1:
        return ys[0]
    pos = (len(ys) - 1) * q
    lo = math.floor(pos)
    hi = math.ceil(pos)
    if lo == hi:
        return ys[lo]
    return ys[lo] * (hi - pos) + ys[hi] * (pos - lo)


def fmt(v: float) -> str:
    if math.isnan(v):
        return "—"
    if abs(v) >= 1000:
        return f"{v:,.0f}"
    if abs(v) >= 100:
        return f"{v:.1f}"
    return f"{v:.2f}"


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("input", type=Path)
    ap.add_argument("--json-out", type=Path)
    ap.add_argument("--markdown-out", type=Path)
    ap.add_argument("--expect-trials", type=int)
    args = ap.parse_args()

    rows = []
    with args.input.open() as f:
        for lineno, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError as e:
                raise SystemExit(f"{args.input}:{lineno}: {e}")

    if not rows:
        raise SystemExit("no measurements")

    grouped: dict[tuple, list[dict]] = defaultdict(list)
    for r in rows:
        key = (
            r.get("format_version", 1),
            r.get("lane", "kv"),
            r.get("scenario", "legacy"),
            r["engine"],
            r["engine_version"],
            r["durability"],
            r["workload"],
            r["records"],
            r["ops_requested"],
            r.get("value_bytes", r.get("payload_bytes")),
            r.get("value_pattern", "pseudo-random"),
            r.get("key_bytes", 8),
            r.get("key_shape", "sequential"),
            r.get("access_pattern", "auto"),
            r.get("miss_percent", 0),
            r.get("write_pattern", "append"),
            r["txn_size"],
            r.get("scan_len"),
            (r.get("post_workload_settle") or {}).get("requested_ms", 0),
        )
        grouped[key].append(r)

    summary = []
    problems = []
    for key, rs in sorted(grouped.items()):
        (
            format_version,
            lane,
            scenario,
            engine,
            version,
            durability,
            workload,
            records,
            ops_requested,
            value_bytes,
            value_pattern,
            key_bytes,
            key_shape,
            access_pattern,
            miss_percent,
            write_pattern,
            txn_size,
            scan_len,
            settle_ms,
        ) = key
        throughputs = [float(r["ops_per_s"]) for r in rs]
        p99_read = [
            float(r["read_latency"]["p99_us"])
            for r in rs
            if r.get("read_latency", {}).get("count", 0)
        ]
        p99_op = [
            float(r["operation_latency"]["p99_us"])
            for r in rs
            if r.get("operation_latency", {}).get("count", 0)
        ]
        p99_tx = [
            float((r.get("write_txn_latency") or r.get("transaction_latency"))["p99_us"])
            for r in rs
            if (r.get("write_txn_latency") or r.get("transaction_latency") or {}).get("count", 0)
        ]
        trials = sorted({int(r["trial"]) for r in rs})
        if args.expect_trials is not None and len(trials) != args.expect_trials:
            problems.append(
                f"{lane}/{engine}/{durability}/{workload}: "
                f"{len(trials)} trials, expected {args.expect_trials}"
            )
        summary.append(
            {
                "format_version": format_version,
                "lane": lane,
                "scenario": scenario,
                "engine": engine,
                "engine_version": version,
                "durability": durability,
                "workload": workload,
                "records": records,
                "ops_requested": ops_requested,
                "value_bytes": value_bytes,
                "value_pattern": value_pattern,
                "key_bytes": key_bytes,
                "key_shape": key_shape,
                "access_pattern": access_pattern,
                "miss_percent": miss_percent,
                "write_pattern": write_pattern,
                "txn_size": txn_size,
                "scan_len": scan_len,
                "settle_ms": settle_ms,
                "configuration": (
                    f"k={key_bytes}/{key_shape} v={value_bytes}/{value_pattern} "
                    f"access={access_pattern} miss={miss_percent}% "
                    f"write={write_pattern} settle={settle_ms}ms"
                    if lane == "kv"
                    else f"payload={value_bytes} txn={txn_size}"
                ),
                "trials": trials,
                "ops_per_s_median": statistics.median(throughputs),
                "ops_per_s_q1": quantile(throughputs, 0.25),
                "ops_per_s_q3": quantile(throughputs, 0.75),
                "ops_per_s_min": min(throughputs),
                "ops_per_s_max": max(throughputs),
                "p99_read_us_median": statistics.median(p99_read) if p99_read else None,
                "p99_operation_us_median": statistics.median(p99_op) if p99_op else None,
                "p99_write_txn_us_median": statistics.median(p99_tx) if p99_tx else None,
                "db_bytes_median": statistics.median([int(r["db_bytes"]) for r in rs]),
                "peak_rss_kib_median": statistics.median([int(r["peak_rss_kib"]) for r in rs if "peak_rss_kib" in r]) if any("peak_rss_kib" in r for r in rs) else None,
                "prefill_s_median": statistics.median([float(r["prefill_s"]) for r in rs if "prefill_s" in r]) if any("prefill_s" in r for r in rs) else None,
                "cpu_ns_per_op_median": statistics.median([
                    float(r.get("measured_process", {}).get("cpu_runtime_ns", 0)) / max(int(r["ops_completed"]), 1)
                    for r in rs
                ]) if any("measured_process" in r for r in rs) else None,
                "runqueue_wait_fraction_median": statistics.median([
                    float(r.get("measured_process", {}).get("runqueue_wait_fraction_of_wall", 0.0))
                    for r in rs
                ]) if any("measured_process" in r for r in rs) else None,
                "read_bytes_per_op_median": statistics.median([
                    float(r.get("measured_process", {}).get("read_bytes", 0)) / max(int(r["ops_completed"]), 1)
                    for r in rs
                ]) if any("measured_process" in r for r in rs) else None,
                "write_bytes_per_op_median": statistics.median([
                    float(r.get("measured_process", {}).get("write_bytes", 0)) / max(int(r["ops_completed"]), 1)
                    for r in rs
                ]) if any("measured_process" in r for r in rs) else None,
                "major_faults_per_kop_median": statistics.median([
                    1000.0 * float(r.get("measured_process", {}).get("majflt", 0)) / max(int(r["ops_completed"]), 1)
                    for r in rs
                ]) if any("measured_process" in r for r in rs) else None,
                "io_psi_full_fraction_median": statistics.median([
                    float(r.get("measured_system_delta", {}).get("psi_io_full_us", 0)) / max(float(r["elapsed_s"]) * 1_000_000.0, 1.0)
                    for r in rs
                ]) if any("measured_system_delta" in r for r in rs) else None,
                "swap_activity_pages_median": statistics.median([
                    int(r.get("measured_system_delta", {}).get("pswpin", 0)) + int(r.get("measured_system_delta", {}).get("pswpout", 0))
                    for r in rs
                ]) if any("measured_system_delta" in r for r in rs) else None,
                "settle_cpu_ns_median": statistics.median([
                    int(r["post_workload_settle"]["process"]["cpu_runtime_ns"])
                    for r in rs if r.get("post_workload_settle")
                ]) if any(r.get("post_workload_settle") for r in rs) else None,
                "settle_write_bytes_median": statistics.median([
                    int(r["post_workload_settle"]["process"]["write_bytes"])
                    for r in rs if r.get("post_workload_settle")
                ]) if any(r.get("post_workload_settle") for r in rs) else None,
                "settle_db_delta_bytes_median": statistics.median([
                    int(r["post_workload_settle"]["db_bytes_after"]) - int(r["post_workload_settle"]["db_bytes_before"])
                    for r in rs if r.get("post_workload_settle")
                ]) if any(r.get("post_workload_settle") for r in rs) else None,
                "durability_mapping": sorted({r["durability_mapping"] for r in rs}),
            }
        )

    md = []
    if problems:
        md += ["# Completeness warnings", ""]
        md += [f"- {p}" for p in problems]
        md += [""]

    for lane in sorted({s["lane"] for s in summary}):
        md += [f"# {lane} lane", ""]
        for durability in sorted({s["durability"] for s in summary if s["lane"] == lane}):
            md += [f"## {durability}", ""]
            md += [
                "| schema | scenario | workload | config | engine | trials | median ops/s | IQR ops/s | median p99 read/op us | median p99 write-txn us | DB MiB | peak RSS MiB | prefill s | CPU ns/op | rq wait %wall | read B/op | write B/op | IO PSI full %wall | swap pages |",
                "|---:|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
            ]
            block = [
                s for s in summary
                if s["lane"] == lane and s["durability"] == durability
            ]
            block.sort(key=lambda s: (s["scenario"], s["workload"], -s["ops_per_s_median"]))
            for s in block:
                p99 = s["p99_read_us_median"]
                if p99 is None:
                    p99 = s["p99_operation_us_median"]
                md.append(
                    "| {schema} | {scenario} | {workload} | {config} | {engine} {version} | {trials} | {median} | {q1}–{q3} | {p99} | {tx} | {mib} | {rss} | {prefill} | {cpu} | {rq} | {readb} | {writeb} | {iopsi} | {swap} |".format(
                        schema=s["format_version"],
                        scenario=s["scenario"],
                        workload=s["workload"],
                        config=s["configuration"],
                        engine=s["engine"],
                        version=s["engine_version"],
                        trials=len(s["trials"]),
                        median=fmt(s["ops_per_s_median"]),
                        q1=fmt(s["ops_per_s_q1"]),
                        q3=fmt(s["ops_per_s_q3"]),
                        p99=fmt(p99) if p99 is not None else "—",
                        tx=fmt(s["p99_write_txn_us_median"]) if s["p99_write_txn_us_median"] is not None else "—",
                        mib=fmt(s["db_bytes_median"] / (1024 * 1024)),
                        rss=fmt(s["peak_rss_kib_median"] / 1024) if s["peak_rss_kib_median"] is not None else "—",
                        prefill=fmt(s["prefill_s_median"]) if s["prefill_s_median"] is not None else "—",
                        cpu=fmt(s["cpu_ns_per_op_median"]) if s["cpu_ns_per_op_median"] is not None else "—",
                        rq=fmt(100.0 * s["runqueue_wait_fraction_median"]) if s["runqueue_wait_fraction_median"] is not None else "—",
                        readb=fmt(s["read_bytes_per_op_median"]) if s["read_bytes_per_op_median"] is not None else "—",
                        writeb=fmt(s["write_bytes_per_op_median"]) if s["write_bytes_per_op_median"] is not None else "—",
                        iopsi=fmt(100.0 * s["io_psi_full_fraction_median"]) if s["io_psi_full_fraction_median"] is not None else "—",
                        swap=fmt(s["swap_activity_pages_median"]) if s["swap_activity_pages_median"] is not None else "—",
                    )
                )
            md.append("")

    text = "\n".join(md)
    if args.markdown_out:
        args.markdown_out.write_text(text + "\n")
    else:
        print(text)

    out = {
        "source": str(args.input),
        "row_count": len(rows),
        "group_count": len(summary),
        "problems": problems,
        "groups": summary,
    }
    if args.json_out:
        args.json_out.write_text(json.dumps(out, indent=2) + "\n")



if __name__ == "__main__":
    main()
