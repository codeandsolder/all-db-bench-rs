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
from typing import Any


def median(xs: list[float]) -> float | None:
    return statistics.median(xs) if xs else None


def ratio(value: float | None, base: float | None) -> float | None:
    if value is None or base is None or base == 0:
        return None
    return value / base


def device_bytes(row: dict[str, Any]) -> tuple[int, int]:
    counters = row.get("compression_storage", {}).get("storage_counters", {})
    before = counters.get("before", {}).get("devices") or []
    after = counters.get("after", {}).get("devices") or []
    before_by_path = {d.get("path"): d for d in before}
    read_bytes = write_bytes = 0
    for end in after:
        start = before_by_path.get(end.get("path"))
        if not start:
            continue
        a = start.get("stat") or []
        b = end.get("stat") or []
        if len(a) < 7 or len(b) < 7:
            continue
        # Linux block stat fields 3 and 7 are 512-byte sectors, independent of logical block size.
        read_bytes += max(0, int(b[2]) - int(a[2])) * 512
        write_bytes += max(0, int(b[6]) - int(a[6])) * 512
    return read_bytes, write_bytes


def nested_delta(row: dict[str, Any], section: str, prop: str) -> float | None:
    snapshots = row.get("compression_storage", {}).get(section, {})
    before = snapshots.get("before", {})
    after = snapshots.get("after", {})
    if section == "dataset_snapshot":
        before = before.get("zfs_dataset") or {}
        after = after.get("zfs_dataset") or {}
    elif section == "storage_counters":
        before = before.get("zfs_pool_io") or {}
        after = after.get("zfs_pool_io") or {}
    a = before.get(prop)
    b = after.get(prop)
    if not isinstance(a, (int, float)) or not isinstance(b, (int, float)):
        return None
    return max(0.0, float(b) - float(a))


def logical_live_bytes(row: dict[str, Any]) -> int:
    live_records = int(row["records"])
    if row.get("workload") == "write-burst" and row.get("write_pattern") == "append":
        live_records += int(row.get("writes", 0))
    return live_records * (int(row.get("key_bytes", 8)) + int(row["value_bytes"]))


def group_key(row: dict[str, Any]) -> tuple[Any, ...]:
    return (
        row["engine"], row["engine_version"], row["engine_compression"], row["workload"],
        row["value_bytes"], row["value_pattern"], row["durability"], row["records"],
        row["ops_requested"], (row.get("post_workload_settle") or {}).get("requested_ms", 0),
    )


def summarize(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    groups: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        groups[group_key(row)].append(row)

    out: list[dict[str, Any]] = []
    for key, rs in sorted(groups.items()):
        engine, version, codec, workload, value_bytes, pattern, durability, records, ops, settle_ms = key
        cpu_ns_op = [
            float(r["measured_process"]["cpu_runtime_ns"]) / max(1, int(r["ops_completed"]))
            for r in rs
        ]
        apparent = [float(r["compression_storage"]["footprint"]["apparent_bytes"]) for r in rs]
        allocated = [float(r["compression_storage"]["footprint"]["allocated_block_bytes"]) for r in rs]
        logical = [float(logical_live_bytes(r)) for r in rs]
        dev = [device_bytes(r) for r in rs]
        process_read = [float(r["measured_process"].get("read_bytes", 0)) for r in rs]
        process_write = [float(r["measured_process"].get("write_bytes", 0)) for r in rs]
        zfs_used = [x for r in rs if (x := nested_delta(r, "dataset_snapshot", "used")) is not None]
        zfs_logical = [x for r in rs if (x := nested_delta(r, "dataset_snapshot", "logicalused")) is not None]
        zfs_direct_write = [x for r in rs if (x := nested_delta(r, "storage_counters", "direct_write_bytes")) is not None]
        zfs_arc_write = [x for r in rs if (x := nested_delta(r, "storage_counters", "arc_write_bytes")) is not None]
        completed = [max(1, int(r["ops_completed"])) for r in rs]
        item = {
            "engine": engine,
            "engine_version": version,
            "engine_compression": codec,
            "workload": workload,
            "durability": durability,
            "records": records,
            "ops_requested": ops,
            "value_bytes": value_bytes,
            "value_pattern": pattern,
            "settle_ms": settle_ms,
            "trials": sorted(int(r["trial"]) for r in rs),
            "ops_per_s_median": median([float(r["ops_per_s"]) for r in rs]),
            "cpu_ns_per_op_median": median(cpu_ns_op),
            "db_apparent_bytes_median": median(apparent),
            "db_allocated_block_bytes_median": median(allocated),
            "logical_live_kv_bytes_median": median(logical),
            "logical_live_per_allocated_byte_median": median([
                l / a if a else math.nan for l, a in zip(logical, allocated)
            ]),
            "lifecycle_device_read_bytes_median": median([float(x[0]) for x in dev]),
            "lifecycle_device_write_bytes_median": median([float(x[1]) for x in dev]),
            "zfs_dataset_used_delta_median": median(zfs_used),
            "zfs_dataset_logicalused_delta_median": median(zfs_logical),
            "zfs_pool_direct_write_bytes_delta_median": median(zfs_direct_write),
            "zfs_pool_arc_write_bytes_delta_median": median(zfs_arc_write),
            "measured_process_read_bytes_per_op_median": median([
                b / n for b, n in zip(process_read, completed)
            ]),
            "measured_process_write_bytes_per_op_median": median([
                b / n for b, n in zip(process_write, completed)
            ]),
            "p99_read_us_median": median([
                float(r["read_latency"]["p99_us"]) for r in rs if r["read_latency"]["count"]
            ]),
            "p99_write_txn_us_median": median([
                float(r["write_txn_latency"]["p99_us"]) for r in rs if r["write_txn_latency"]["count"]
            ]),
        }
        out.append(item)

    baselines = {
        (x["engine"], x["workload"], x["value_bytes"], x["value_pattern"], x["durability"], x["records"], x["ops_requested"], x["settle_ms"]): x
        for x in out if x["engine_compression"] == "none"
    }
    for item in out:
        base = baselines.get((
            item["engine"], item["workload"], item["value_bytes"], item["value_pattern"],
            item["durability"], item["records"], item["ops_requested"], item["settle_ms"],
        ))
        item["vs_none"] = None if base is None else {
            "throughput_ratio": ratio(item["ops_per_s_median"], base["ops_per_s_median"]),
            "cpu_ns_per_op_ratio": ratio(item["cpu_ns_per_op_median"], base["cpu_ns_per_op_median"]),
            "apparent_bytes_ratio": ratio(item["db_apparent_bytes_median"], base["db_apparent_bytes_median"]),
            "allocated_block_bytes_ratio": ratio(item["db_allocated_block_bytes_median"], base["db_allocated_block_bytes_median"]),
            "device_write_bytes_ratio": ratio(item["lifecycle_device_write_bytes_median"], base["lifecycle_device_write_bytes_median"]),
            "zfs_dataset_used_ratio": ratio(item["zfs_dataset_used_delta_median"], base["zfs_dataset_used_delta_median"]),
        }
    return out


def fnum(x: float | None, digits: int = 2) -> str:
    return "—" if x is None or math.isnan(x) else f"{x:.{digits}f}"


def markdown(summary: list[dict[str, Any]]) -> str:
    lines = [
        "# KV compression summary",
        "",
        "Ratios are relative to the same engine/workload/value-shape case with engine compression `none`. ",
        "Allocated-block bytes are filesystem block accounting, not guaranteed physical-media bytes; lifecycle device bytes come from block counters.",
        "",
        "| Engine | Codec | Workload | Value | Shape | ops/s | CPU ns/op | alloc MiB | logical/alloc | thr/none | CPU/none | alloc/none |",
        "|---|---|---|---:|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for x in summary:
        vn = x.get("vs_none") or {}
        lines.append(
            f"| {x['engine']} | {x['engine_compression']} | {x['workload']} | {x['value_bytes']} | {x['value_pattern']} | "
            f"{fnum(x['ops_per_s_median'], 0)} | {fnum(x['cpu_ns_per_op_median'], 0)} | "
            f"{fnum((x['db_allocated_block_bytes_median'] or 0)/(1024*1024), 2)} | "
            f"{fnum(x['logical_live_per_allocated_byte_median'], 2)} | {fnum(vn.get('throughput_ratio'), 3)} | "
            f"{fnum(vn.get('cpu_ns_per_op_ratio'), 3)} | {fnum(vn.get('allocated_block_bytes_ratio'), 3)} |"
        )
    lines.append("")
    return "\n".join(lines)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("input", type=Path)
    ap.add_argument("--json-out", type=Path, required=True)
    ap.add_argument("--markdown-out", type=Path, required=True)
    args = ap.parse_args()
    rows = [json.loads(line) for line in args.input.read_text().splitlines() if line.strip()]
    if not rows:
        raise SystemExit("no compression measurements")
    summary = summarize(rows)
    payload = {"format_version": 1, "lane": "kv-compression", "row_count": len(rows), "group_count": len(summary), "groups": summary}
    args.json_out.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
    args.markdown_out.write_text(markdown(summary))


if __name__ == "__main__":
    main()
