#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

TESTS = (
    "direct-seqread-1m-q1",
    "direct-seqwrite-1m-q1",
    "direct-randread-4k-q1",
    "direct-randwrite-4k-q1",
    "direct-randrw70-4k-q1",
    "direct-randread-4k-q16",
    "direct-randwrite-4k-q16",
    "buffered-randread-4k-q1",
    "buffered-seqread-1m-q1",
    "fdatasync-write-4k-q1",
)


def number(value: Any) -> float:
    try:
        return float(value or 0)
    except (TypeError, ValueError):
        return 0.0


def bw_bytes(section: dict[str, Any]) -> float:
    if "bw_bytes" in section:
        return number(section.get("bw_bytes"))
    return number(section.get("bw")) * 1024.0


def percentile(lat: dict[str, Any], key: str) -> float | None:
    values = lat.get("percentile") or {}
    if key in values:
        return number(values[key])
    # fio has used both six and variable decimal places in JSON keys.
    target = number(key)
    for name, value in values.items():
        if abs(number(name) - target) < 1e-9:
            return number(value)
    return None


def latency(section: dict[str, Any]) -> dict[str, float | None] | None:
    lat = section.get("clat_ns") or section.get("lat_ns") or {}
    if not lat:
        return None
    return {
        "mean_ns": number(lat.get("mean")),
        "p50_ns": percentile(lat, "50.000000"),
        "p95_ns": percentile(lat, "95.000000"),
        "p99_ns": percentile(lat, "99.000000"),
        "p99_9_ns": percentile(lat, "99.900000"),
        "max_ns": number(lat.get("max")),
    }


def io_section(section: dict[str, Any]) -> dict[str, Any]:
    return {
        "ios": int(number(section.get("total_ios"))),
        "iops": number(section.get("iops")),
        "bw_bytes_per_s": bw_bytes(section),
        "latency": latency(section),
    }


def load_test(run_dir: Path, name: str) -> dict[str, Any]:
    path = run_dir / f"{name}.json"
    if not path.is_file():
        raise FileNotFoundError(f"missing fio result: {path}")
    raw = json.loads(path.read_text())
    jobs = raw.get("jobs") or []
    if len(jobs) != 1:
        raise ValueError(f"{path}: expected exactly one fio job, got {len(jobs)}")
    job = jobs[0]
    read = io_section(job.get("read") or {})
    write = io_section(job.get("write") or {})
    sync = job.get("sync") or {}
    sync_lat = latency(sync) if sync else None
    return {
        "name": name,
        "read": read,
        "write": write,
        "total_iops": read["iops"] + write["iops"],
        "total_bw_bytes_per_s": read["bw_bytes_per_s"] + write["bw_bytes_per_s"],
        "sync_ios": int(number(sync.get("total_ios"))) if sync else 0,
        "sync_latency": sync_lat,
    }


def fmt_rate(value: float) -> str:
    if value >= 1_000_000_000:
        return f"{value / 1_000_000_000:.2f} GB/s"
    if value >= 1_000_000:
        return f"{value / 1_000_000:.2f} MB/s"
    if value >= 1_000:
        return f"{value / 1_000:.2f} kB/s"
    return f"{value:.0f} B/s"


def fmt_iops(value: float) -> str:
    if value >= 1_000_000:
        return f"{value / 1_000_000:.2f}M"
    if value >= 1_000:
        return f"{value / 1_000:.2f}k"
    return f"{value:.1f}"


def fmt_us(value: float | None) -> str:
    return "-" if value is None else f"{value / 1000.0:.1f}"


def markdown(summary: dict[str, Any]) -> str:
    rows = [
        "# Raw storage calibration",
        "",
        "These are backing-storage calibration measurements, not database scores.",
        "",
        "| fio case | IOPS | bandwidth | read p99 µs | write p99 µs | sync p99 µs |",
        "|---|---:|---:|---:|---:|---:|",
    ]
    for test in summary["tests"]:
        read_p99 = (test["read"].get("latency") or {}).get("p99_ns")
        write_p99 = (test["write"].get("latency") or {}).get("p99_ns")
        sync_p99 = (test.get("sync_latency") or {}).get("p99_ns")
        rows.append(
            f"| {test['name']} | {fmt_iops(test['total_iops'])} | "
            f"{fmt_rate(test['total_bw_bytes_per_s'])} | {fmt_us(read_p99)} | "
            f"{fmt_us(write_p99)} | {fmt_us(sync_p99)} |"
        )
    cal = summary["calibration"]
    rows += [
        "",
        f"Calibrated 4 KiB QD1 70/30 random-I/O rate: **{fmt_iops(cal['randrw70_4k_q1_iops'])} IOPS**.",
        "",
    ]
    return "\n".join(rows)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("--json-out", type=Path)
    parser.add_argument("--markdown-out", type=Path)
    args = parser.parse_args()

    tests = [load_test(args.run_dir, name) for name in TESTS]
    by_name = {row["name"]: row for row in tests}
    baseline_iops = float(by_name["direct-randrw70-4k-q1"]["total_iops"])
    if baseline_iops <= 0:
        raise ValueError("direct-randrw70-4k-q1 reported no I/O")

    summary = {
        "format_version": 1,
        "lane": "io-calibration",
        "tests": tests,
        "calibration": {"randrw70_4k_q1_iops": baseline_iops},
    }
    text = json.dumps(summary, indent=2, sort_keys=True) + "\n"
    md = markdown(summary)
    if args.json_out:
        args.json_out.write_text(text)
    else:
        print(text, end="")
    if args.markdown_out:
        args.markdown_out.write_text(md)


if __name__ == "__main__":
    main()
