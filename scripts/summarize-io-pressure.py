#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
import math
import re
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable

from io_evidence import fio_p99_us, load_storage_delta, ratio, validate_fio_job

SCENARIO_RE = re.compile(r"^io-pressure-(\d+)pct$")


def median(values: Iterable[float]) -> float | None:
    xs = list(values)
    return statistics.median(xs) if xs else None



def load_case(
    case_path: Path, pressure_path: Path, baseline_iops: float, expected_bs: int,
    baseline_read_iops: float, baseline_write_iops: float,
    read_fraction: float, write_fraction: float,
) -> dict[str, Any]:
    db = json.loads(case_path.read_text())
    pressure = json.loads(pressure_path.read_text())
    match = SCENARIO_RE.match(str(db.get("scenario", "")))
    if not match:
        raise ValueError(f"unexpected scenario {db.get('scenario')!r}")
    pct = int(match.group(1))
    sidecar_pct = int(pressure.get("pressure_percent", -1))
    sidecar_bs = int(pressure.get("bs_bytes", 0))
    if sidecar_pct != pct:
        raise ValueError(f"pressure sidecar percent {sidecar_pct} != scenario {pct}")
    if sidecar_bs != expected_bs:
        raise ValueError(f"pressure sidecar block size {sidecar_bs} != run support {expected_bs}")
    for field, expected in (
        ("baseline_iops", baseline_iops),
        ("baseline_read_iops", baseline_read_iops),
        ("baseline_write_iops", baseline_write_iops),
        ("read_fraction", read_fraction),
        ("write_fraction", write_fraction),
    ):
        actual = float(pressure.get(field, 0.0))
        if not math.isclose(actual, expected, rel_tol=1e-9, abs_tol=1e-6):
            raise ValueError(f"pressure sidecar {field} {actual} != run support {expected}")

    target_iops = float(pressure.get("target_iops", 0.0))
    target_read_iops = float(pressure.get("target_read_iops", 0.0))
    target_write_iops = float(pressure.get("target_write_iops", 0.0))
    pressure_read_iops = pressure_write_iops = pressure_bw_bytes = 0.0
    pressure_read_bytes = pressure_write_bytes = 0.0
    pressure_read_p99_us = pressure_write_p99_us = None
    fio_error = 0
    jobs = pressure.get("jobs") or []
    if pct > 0:
        if len(jobs) != 2:
            raise ValueError(f"nonzero pressure case has {len(jobs)} fio jobs, expected 2")
        read_jobs = [j for j in jobs if (j.get("job options") or {}).get("rw") == "randread"]
        write_jobs = [j for j in jobs if (j.get("job options") or {}).get("rw") == "randwrite"]
        if len(read_jobs) != 1 or len(write_jobs) != 1:
            raise ValueError("pressure sidecar must contain exactly one randread and one randwrite job")
        read_job = read_jobs[0]
        write_job = write_jobs[0]
        validate_fio_job(
            read_job, rw="randread", bs_bytes=expected_bs, blockalign_bytes=expected_bs,
            ioengine="psync", direct=1,
        )
        validate_fio_job(
            write_job, rw="randwrite", bs_bytes=expected_bs, blockalign_bytes=expected_bs,
            ioengine="psync", direct=1,
        )
        fio_error = max(int(read_job.get("error", 0)), int(write_job.get("error", 0)))
        read = read_job.get("read", {})
        write = write_job.get("write", {})
        pressure_read_iops = float(read.get("iops", 0.0))
        pressure_write_iops = float(write.get("iops", 0.0))
        pressure_bw_bytes = float(read.get("bw_bytes", 0.0)) + float(write.get("bw_bytes", 0.0))
        pressure_read_bytes = float(read.get("io_bytes", 0.0))
        pressure_write_bytes = float(write.get("io_bytes", 0.0))
        pressure_read_p99_us = fio_p99_us(read)
        pressure_write_p99_us = fio_p99_us(write)

    if pct == 0 and (target_iops != 0 or target_read_iops != 0 or target_write_iops != 0):
        raise ValueError("zero-pressure case has nonzero target IOPS")
    if pct > 0 and (target_iops <= 0 or target_read_iops <= 0 or target_write_iops <= 0):
        raise ValueError("nonzero pressure case has nonpositive target IOPS")
    if pct > 0 and not math.isclose(target_iops, target_read_iops + target_write_iops, rel_tol=0, abs_tol=1e-6):
        raise ValueError("pressure target_iops does not equal directional target sum")

    delivered_iops = pressure_read_iops + pressure_write_iops
    ops_completed = max(int(db.get("ops_completed", 0)), 1)
    proc = db.get("measured_process", {})
    system = db.get("measured_system_delta", {})
    system_wall_ns = max(float(system.get("accounting_wall_ns", 0)), 1.0)

    return {
        "case_id": case_path.stem,
        "format_version": db.get("format_version"),
        "engine": db.get("engine"),
        "engine_version": db.get("engine_version"),
        "durability": db.get("durability"),
        "workload": db.get("workload"),
        "records": db.get("records"),
        "ops_requested": db.get("ops_requested"),
        "value_bytes": db.get("value_bytes"),
        "txn_size": db.get("txn_size"),
        "trial": db.get("trial"),
        "pressure_percent": pct,
        "pressure_bs_bytes": int(pressure.get("bs_bytes", 0)),
        "pressure_target_iops": target_iops,
        "pressure_target_read_iops": target_read_iops,
        "pressure_target_write_iops": target_write_iops,
        "pressure_delivered_iops": delivered_iops,
        "pressure_delivered_read_iops": pressure_read_iops,
        "pressure_delivered_write_iops": pressure_write_iops,
        "pressure_delivered_vs_target": ratio(delivered_iops, target_iops) if pct > 0 else None,
        "pressure_delivered_read_vs_target": ratio(pressure_read_iops, target_read_iops) if pct > 0 else None,
        "pressure_delivered_write_vs_target": ratio(pressure_write_iops, target_write_iops) if pct > 0 else None,
        "pressure_delivered_vs_baseline": ratio(delivered_iops, baseline_iops) if pct > 0 else 0.0,
        "pressure_delivered_bw_mbps": pressure_bw_bytes / 1_000_000.0,
        "pressure_read_bytes": pressure_read_bytes,
        "pressure_write_bytes": pressure_write_bytes,
        "pressure_read_p99_us": pressure_read_p99_us,
        "pressure_write_p99_us": pressure_write_p99_us,
        "pressure_fio_error": fio_error,
        "db_ops_per_s": float(db.get("ops_per_s", 0.0)),
        "db_read_p99_us": float(db.get("read_latency", {}).get("p99_us", 0.0)),
        "db_write_p99_us": float(db.get("write_txn_latency", {}).get("p99_us", 0.0)),
        "db_cpu_ns_per_op": float(proc.get("cpu_runtime_ns", 0)) / ops_completed,
        "db_runqueue_wait_fraction": float(proc.get("runqueue_wait_fraction_of_wall", 0.0)),
        "db_io_psi_full_fraction": float(system.get("psi_io_full_us", 0)) * 1000.0 / system_wall_ns,
    }



def identity(c: dict[str, Any]) -> tuple[Any, ...]:
    return (
        c["format_version"], c["engine"], c["engine_version"], c["durability"],
        c["workload"], c["records"], c["ops_requested"], c["value_bytes"], c["txn_size"],
    )


def enrich_ratios(cases: list[dict[str, Any]], problems: list[str]) -> None:
    baselines: dict[tuple[Any, ...], dict[str, Any]] = {}
    for c in cases:
        if c["pressure_percent"] == 0:
            key = identity(c) + (c["trial"],)
            if key in baselines:
                problems.append(f"duplicate zero-pressure baseline for {key}")
            baselines[key] = c

    for c in cases:
        key = identity(c) + (c["trial"],)
        base = baselines.get(key)
        if base is None:
            problems.append(
                f"missing zero-pressure baseline for {c['engine']} {c['durability']} {c['workload']} trial={c['trial']}"
            )
            c["db_throughput_ratio_vs_zero"] = None
            c["db_read_p99_ratio_vs_zero"] = None
            c["db_write_p99_ratio_vs_zero"] = None
            continue
        c["db_throughput_ratio_vs_zero"] = ratio(c["db_ops_per_s"], base["db_ops_per_s"])
        c["db_read_p99_ratio_vs_zero"] = ratio(c["db_read_p99_us"], base["db_read_p99_us"])
        c["db_write_p99_ratio_vs_zero"] = ratio(c["db_write_p99_us"], base["db_write_p99_us"])


def aggregate(cases: list[dict[str, Any]]) -> list[dict[str, Any]]:
    groups: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
    for c in cases:
        groups[identity(c) + (c["pressure_percent"], c["pressure_bs_bytes"])].append(c)

    fields = [
        "pressure_target_iops", "pressure_target_read_iops", "pressure_target_write_iops",
        "pressure_delivered_iops", "pressure_delivered_read_iops", "pressure_delivered_write_iops",
        "pressure_delivered_bw_mbps", "pressure_delivered_vs_target",
        "pressure_delivered_read_vs_target", "pressure_delivered_write_vs_target",
        "pressure_delivered_vs_baseline",
        "pressure_read_p99_us", "pressure_write_p99_us",
        "db_ops_per_s", "db_throughput_ratio_vs_zero", "db_read_p99_us",
        "db_read_p99_ratio_vs_zero", "db_write_p99_us", "db_write_p99_ratio_vs_zero",
        "db_cpu_ns_per_op", "db_runqueue_wait_fraction", "db_io_psi_full_fraction",
        "storage_accounting_s", "storage_read_ios", "storage_write_ios",
        "storage_read_bytes", "storage_write_bytes", "storage_discard_bytes",
        "storage_flushes", "storage_io_ms", "storage_weighted_io_ms",
        "zfs_arc_hits", "zfs_arc_misses", "storage_read_mbps", "storage_write_mbps",
        "zfs_arc_hit_fraction", "zfs_direct_read_ios", "zfs_direct_read_bytes",
        "zfs_direct_write_ios", "zfs_direct_write_bytes",
        "pressure_storage_accounting_s", "pressure_zfs_direct_read_ios",
        "pressure_zfs_direct_read_bytes", "pressure_zfs_direct_write_ios",
        "pressure_zfs_direct_write_bytes",
        "zfs_direct_read_bytes_per_pressure_read_byte",
        "zfs_direct_write_bytes_per_pressure_write_byte",
    ]
    out = []
    for key, rows in sorted(groups.items(), key=lambda item: tuple(str(x) for x in item[0])):
        first = rows[0]
        g = {name: first[name] for name in [
            "format_version", "engine", "engine_version", "durability", "workload",
            "records", "ops_requested", "value_bytes", "txn_size", "pressure_percent", "pressure_bs_bytes",
        ]}
        g["trials"] = len(rows)
        for field in fields:
            vals = [float(r[field]) for r in rows if r.get(field) is not None and math.isfinite(float(r[field]))]
            g[field + "_median"] = median(vals)
            g[field + "_min"] = min(vals) if vals else None
            g[field + "_max"] = max(vals) if vals else None
        out.append(g)
    return out


def fmt(value: Any, digits: int = 3) -> str:
    if value is None:
        return "—"
    if isinstance(value, float):
        if not math.isfinite(value):
            return str(value)
        return f"{value:.{digits}f}"
    return str(value)


def main() -> None:
    ap = argparse.ArgumentParser(description="Join I/O-pressure sidecars to database results and summarize delivered pressure")
    ap.add_argument("run_dir", type=Path)
    ap.add_argument("--expect-trials", type=int)
    ap.add_argument("--json-out", type=Path)
    ap.add_argument("--markdown-out", type=Path)
    args = ap.parse_args()

    run_dir = args.run_dir
    support = json.loads((run_dir / "support.json").read_text())
    protocol = int(support.get("calibration_protocol_version", 0))
    if protocol != 4:
        raise SystemExit(f"unsupported I/O calibration protocol {protocol}; expected 4")
    baseline_iops = float(support["baseline_iops"])
    baseline_read_iops = float(support["baseline_read_iops"])
    baseline_write_iops = float(support["baseline_write_iops"])
    read_fraction = float(support["read_fraction"])
    write_fraction = float(support["write_fraction"])
    expected_bs = int(support["pressure_bs_bytes"])
    problems: list[str] = []
    warnings: list[str] = []
    cases: list[dict[str, Any]] = []

    case_paths = sorted((run_dir / "cases").glob("*.json"))
    pressure_paths = {p.stem: p for p in (run_dir / "pressure").glob("*.json")}
    storage_paths = {p.stem: p for p in (run_dir / "storage").glob("*.json")}
    pressure_storage_paths = {p.stem: p for p in (run_dir / "pressure-storage").glob("*.json")}
    for case_path in case_paths:
        pressure_path = pressure_paths.get(case_path.stem)
        storage_path = storage_paths.get(case_path.stem)
        if pressure_path is None:
            problems.append(f"missing pressure sidecar for {case_path.stem}")
            continue
        if storage_path is None:
            problems.append(f"missing storage sidecar for {case_path.stem}")
            continue
        try:
            c = load_case(
                case_path, pressure_path, baseline_iops, expected_bs,
                baseline_read_iops, baseline_write_iops, read_fraction, write_fraction,
            )
            c.update(load_storage_delta(storage_path))
            if c["pressure_percent"] > 0:
                pressure_storage_path = pressure_storage_paths.get(case_path.stem)
                if pressure_storage_path is None:
                    raise ValueError("missing pressure-lifetime storage sidecar")
                pressure_storage = load_storage_delta(pressure_storage_path)
                c["pressure_storage_accounting_s"] = pressure_storage["storage_accounting_s"]
                c["pressure_zfs_direct_read_ios"] = pressure_storage["zfs_direct_read_ios"]
                c["pressure_zfs_direct_read_bytes"] = pressure_storage["zfs_direct_read_bytes"]
                c["pressure_zfs_direct_write_ios"] = pressure_storage["zfs_direct_write_ios"]
                c["pressure_zfs_direct_write_bytes"] = pressure_storage["zfs_direct_write_bytes"]
                c["zfs_direct_read_bytes_per_pressure_read_byte"] = ratio(
                    float(c["pressure_zfs_direct_read_bytes"]) if c["pressure_zfs_direct_read_bytes"] is not None else None,
                    float(c["pressure_read_bytes"]),
                )
                c["zfs_direct_write_bytes_per_pressure_write_byte"] = ratio(
                    float(c["pressure_zfs_direct_write_bytes"]) if c["pressure_zfs_direct_write_bytes"] is not None else None,
                    float(c["pressure_write_bytes"]),
                )
            else:
                c["pressure_storage_accounting_s"] = None
                c["pressure_zfs_direct_read_ios"] = None
                c["pressure_zfs_direct_read_bytes"] = None
                c["pressure_zfs_direct_write_ios"] = None
                c["pressure_zfs_direct_write_bytes"] = None
                c["zfs_direct_read_bytes_per_pressure_read_byte"] = None
                c["zfs_direct_write_bytes_per_pressure_write_byte"] = None
        except (ValueError, KeyError, json.JSONDecodeError) as exc:
            problems.append(f"{case_path.stem}: {exc}")
            continue
        if c["pressure_bs_bytes"] != expected_bs:
            problems.append(
                f"{case_path.stem}: pressure block size {c['pressure_bs_bytes']} != run support {expected_bs}"
            )
        if c["pressure_fio_error"] != 0:
            problems.append(f"{case_path.stem}: fio error={c['pressure_fio_error']}")
        if str(support.get("filesystem")) == "zfs" and c["pressure_percent"] > 0:
            direct_read_ratio = c["zfs_direct_read_bytes_per_pressure_read_byte"]
            direct_write_ratio = c["zfs_direct_write_bytes_per_pressure_write_byte"]
            if direct_read_ratio is None or direct_read_ratio < 0.80:
                problems.append(
                    f"{case_path.stem}: ZFS direct-read evidence covers only {direct_read_ratio!r} of fio pressure reads"
                )
            if direct_write_ratio is None or direct_write_ratio < 0.80:
                problems.append(
                    f"{case_path.stem}: ZFS direct-write evidence covers only {direct_write_ratio!r} of fio pressure writes"
                )
        if c["pressure_percent"] > 0 and c["pressure_delivered_vs_target"] is not None:
            delivery = c["pressure_delivered_vs_target"]
            if delivery < 0.80:
                warnings.append(
                    f"{case_path.stem}: delivered pressure was {100.0 * delivery:.1f}% of target; "
                    "this may be real device contention, but compare requested and delivered pressure when interpreting the DB result"
                )
        cases.append(c)

    completed_stems = {p.stem for p in case_paths}
    extra_pressure = sorted(set(pressure_paths) - completed_stems)
    for stem in extra_pressure:
        warnings.append(f"pressure sidecar has no completed database result: {stem}")
    extra_storage = sorted(set(storage_paths) - completed_stems)
    for stem in extra_storage:
        warnings.append(f"storage sidecar has no completed database result: {stem}")
    extra_pressure_storage = sorted(set(pressure_storage_paths) - completed_stems)
    for stem in extra_pressure_storage:
        warnings.append(f"pressure-lifetime storage sidecar has no completed database result: {stem}")

    enrich_ratios(cases, problems)
    groups = aggregate(cases)
    if args.expect_trials is not None:
        for g in groups:
            if g["trials"] != args.expect_trials:
                problems.append(
                    f"{g['engine']} {g['durability']} {g['workload']} pressure={g['pressure_percent']}%: "
                    f"{g['trials']} trials, expected {args.expect_trials}"
                )

    result = {
        "baseline_iops": baseline_iops,
        "baseline_read_iops": baseline_read_iops,
        "baseline_write_iops": baseline_write_iops,
        "read_fraction": read_fraction,
        "write_fraction": write_fraction,
        "pressure_bs_bytes": expected_bs,
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
            "# I/O-pressure summary",
            "",
            "Delivered pressure is measured by fio. Falling below the requested cap under database contention is a measured outcome, not automatically a harness failure.",
            "",
            "| engine | dur | workload | pressure | trials | delivered IOPS | delivered/target | read/target | write/target | delivered/baseline | fio MB/s | backing read MB/s | backing write MB/s | ARC hit % | DB ops/s | DB throughput/zero | DB read p99/zero | DB write p99/zero | DB IO PSI full % |",
            "|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
        ]
        for g in groups:
            lines.append(
                "| {engine} {version} | {dur} | {workload} | {pct}% | {trials} | {piops} | {ptarget} | {preadtarget} | {pwritetarget} | {pbase} | {pbw} | {sread} | {swrite} | {archit} | {dbops} | {dbratio} | {rratio} | {wratio} | {psi} |".format(
                    engine=g["engine"], version=g["engine_version"], dur=g["durability"], workload=g["workload"],
                    pct=g["pressure_percent"], trials=g["trials"],
                    piops=fmt(g["pressure_delivered_iops_median"]),
                    ptarget=fmt(g["pressure_delivered_vs_target_median"]),
                    preadtarget=fmt(g["pressure_delivered_read_vs_target_median"]),
                    pwritetarget=fmt(g["pressure_delivered_write_vs_target_median"]),
                    pbase=fmt(g["pressure_delivered_vs_baseline_median"]),
                    pbw=fmt(g["pressure_delivered_bw_mbps_median"]),
                    sread=fmt(g["storage_read_mbps_median"]),
                    swrite=fmt(g["storage_write_mbps_median"]),
                    archit=fmt(100.0 * g["zfs_arc_hit_fraction_median"] if g["zfs_arc_hit_fraction_median"] is not None else None),
                    dbops=fmt(g["db_ops_per_s_median"]),
                    dbratio=fmt(g["db_throughput_ratio_vs_zero_median"]),
                    rratio=fmt(g["db_read_p99_ratio_vs_zero_median"]),
                    wratio=fmt(g["db_write_p99_ratio_vs_zero_median"]),
                    psi=fmt(100.0 * g["db_io_psi_full_fraction_median"] if g["db_io_psi_full_fraction_median"] is not None else None),
                )
            )
        args.markdown_out.parent.mkdir(parents=True, exist_ok=True)
        args.markdown_out.write_text("\n".join(lines) + "\n")

    if problems:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
