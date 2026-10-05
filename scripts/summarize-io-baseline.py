#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
import math
from pathlib import Path
from typing import Any

from io_evidence import composite_mixed_iops, fio_p99_us, load_storage_delta, ratio, validate_fio_job


def fmt(v: Any, digits: int = 3) -> str:
    if v is None:
        return "—"
    if isinstance(v, float):
        if not math.isfinite(v):
            return str(v)
        return f"{v:.{digits}f}"
    return str(v)


def main() -> None:
    ap = argparse.ArgumentParser(description="Summarize fio calibration plus backing-device evidence")
    ap.add_argument("run_dir", type=Path)
    ap.add_argument("--json-out", type=Path)
    ap.add_argument("--markdown-out", type=Path)
    args = ap.parse_args()
    run = args.run_dir
    support = json.loads((run / "support.json").read_text())
    if int(support.get("calibration_protocol_version", 0)) != 4:
        raise SystemExit("storage baseline must use calibration protocol 4")
    pressure = support["pressure_calibration"]
    authoritative = {
        str(pressure["read"]["file"]): "read",
        str(pressure["write"]["file"]): "write",
    }
    rows: list[dict[str, Any]] = []
    problems: list[str] = []
    warnings: list[str] = []
    declared_tests = support.get("tests") or []
    if not declared_tests:
        raise SystemExit("storage baseline support.json has no declared tests")
    if len(declared_tests) != len(set(declared_tests)):
        raise SystemExit("storage baseline support.json contains duplicate test filenames")
    for filename in declared_tests:
        fio_path = run / str(filename)
        if not fio_path.is_file():
            problems.append(f"missing declared fio result {filename}")
            continue
        storage_path = run / "storage" / str(filename)
        if not storage_path.is_file():
            problems.append(f"missing storage sidecar for {fio_path.name}")
            continue
        try:
            raw = json.loads(fio_path.read_text())
            jobs = raw.get("jobs") or []
            if len(jobs) != 1:
                raise ValueError(f"expected one fio job, got {len(jobs)}")
            job = jobs[0]
            if int(job.get("error", 0)) != 0:
                raise ValueError(f"fio error={job.get('error')}")
            calibration_direction = authoritative.get(fio_path.name)
            if calibration_direction is not None:
                validate_fio_job(
                    job, rw=str(pressure[calibration_direction]["rw"]),
                    bs_bytes=int(pressure["bs_bytes"]),
                    blockalign_bytes=int(pressure["blockalign_bytes"]),
                    ioengine=str(pressure["ioengine"]), direct=int(pressure["direct"]),
                )
            read = job.get("read", {})
            write = job.get("write", {})
            submitted_read = float(read.get("io_bytes", 0))
            submitted_write = float(write.get("io_bytes", 0))
            storage = load_storage_delta(storage_path)
            direct_read_ratio = ratio(
                float(storage["zfs_direct_read_bytes"]) if storage["zfs_direct_read_bytes"] is not None else None,
                submitted_read,
            )
            direct_write_ratio = ratio(
                float(storage["zfs_direct_write_bytes"]) if storage["zfs_direct_write_bytes"] is not None else None,
                submitted_write,
            )
            if calibration_direction is not None and str(support.get("filesystem")) == "zfs":
                if calibration_direction == "read" and (direct_read_ratio is None or direct_read_ratio < 0.90):
                    problems.append(
                        f"{fio_path.name}: ZFS direct-read bytes cover only {direct_read_ratio!r} of fio submitted reads"
                    )
                if calibration_direction == "write" and (direct_write_ratio is None or direct_write_ratio < 0.90):
                    problems.append(
                        f"{fio_path.name}: ZFS direct-write bytes cover only {direct_write_ratio!r} of fio submitted writes"
                    )
            rows.append({
                "name": fio_path.stem,
                "authoritative_pressure_calibration": calibration_direction,
                "fio_read_iops": float(read.get("iops", 0.0)),
                "fio_write_iops": float(write.get("iops", 0.0)),
                "fio_read_mbps": float(read.get("bw_bytes", 0.0)) / 1_000_000.0,
                "fio_write_mbps": float(write.get("bw_bytes", 0.0)) / 1_000_000.0,
                "fio_read_p99_us": fio_p99_us(read),
                "fio_write_p99_us": fio_p99_us(write),
                "fio_submitted_read_bytes": submitted_read,
                "fio_submitted_write_bytes": submitted_write,
                "backing_read_bytes_per_submitted_read_byte": ratio(float(storage["storage_read_bytes"]), submitted_read),
                "backing_write_bytes_per_submitted_write_byte": ratio(float(storage["storage_write_bytes"]), submitted_write),
                "zfs_direct_read_bytes_per_submitted_read_byte": direct_read_ratio,
                "zfs_direct_write_bytes_per_submitted_write_byte": direct_write_ratio,
                **storage,
                "job_options": job.get("job options") or {},
            })
        except (ValueError, KeyError, json.JSONDecodeError) as exc:
            problems.append(f"{fio_path.name}: {exc}")
    by_name = {row["name"]: row for row in rows}
    read_name = Path(str(pressure["read"]["file"])).stem
    write_name = Path(str(pressure["write"]["file"])).stem
    read_row = by_name.get(read_name)
    write_row = by_name.get(write_name)
    composite = None
    if read_row is None or write_row is None:
        problems.append("missing one or both protocol-v4 directional calibration rows")
    else:
        try:
            composite = composite_mixed_iops(
                float(read_row["fio_read_iops"]), float(write_row["fio_write_iops"]),
                float(pressure["read_fraction"]),
            )
        except ValueError as exc:
            problems.append(f"invalid directional calibration capacity: {exc}")
    result = {
        "support": support,
        "test_count": len(rows),
        "problems": problems,
        "warnings": warnings,
        "pressure_calibration": {
            "read_iops": float(read_row["fio_read_iops"]) if read_row else None,
            "write_iops": float(write_row["fio_write_iops"]) if write_row else None,
            "composite_iops": composite,
            "read_fraction": float(pressure["read_fraction"]),
            "write_fraction": float(pressure["write_fraction"]),
        },
        "tests": rows,
    }
    text = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(text)
    else:
        print(text, end="")
    if args.markdown_out:
        lines = [
            "# Storage calibration summary", "",
            "Backing-device counters are whole-device context and can include unrelated host I/O. Ratios are observational, not process attribution.", "",
            "| test | pressure ref | fio read IOPS | fio write IOPS | fio read MB/s | fio write MB/s | read p99 us | write p99 us | backing read MB/s | backing write MB/s | backing/read submitted | backing/write submitted | ZFS direct/read submitted | ZFS direct/write submitted | ARC hit % |",
            "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
        ]
        for r in rows:
            lines.append(
                "| {name} | {ref} | {ri} | {wi} | {rbw} | {wbw} | {rp99} | {wp99} | {srbw} | {swbw} | {rr} | {wr} | {zdr} | {zdw} | {arc} |".format(
                    name=r["name"], ref=r["authoritative_pressure_calibration"] or "",
                    ri=fmt(r["fio_read_iops"]), wi=fmt(r["fio_write_iops"]),
                    rbw=fmt(r["fio_read_mbps"]), wbw=fmt(r["fio_write_mbps"]),
                    rp99=fmt(r["fio_read_p99_us"]), wp99=fmt(r["fio_write_p99_us"]),
                    srbw=fmt(r["storage_read_mbps"]), swbw=fmt(r["storage_write_mbps"]),
                    rr=fmt(r["backing_read_bytes_per_submitted_read_byte"]),
                    wr=fmt(r["backing_write_bytes_per_submitted_write_byte"]),
                    zdr=fmt(r["zfs_direct_read_bytes_per_submitted_read_byte"]),
                    zdw=fmt(r["zfs_direct_write_bytes_per_submitted_write_byte"]),
                    arc=fmt(100.0 * r["zfs_arc_hit_fraction"] if r["zfs_arc_hit_fraction"] is not None else None),
                )
            )
        args.markdown_out.parent.mkdir(parents=True, exist_ok=True)
        args.markdown_out.write_text("\n".join(lines) + "\n")
    if problems:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
