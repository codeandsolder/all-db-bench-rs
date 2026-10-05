from __future__ import annotations

import json
import math
from pathlib import Path
from typing import Any


def ratio(num: float | None, den: float | None) -> float | None:
    if num is None or den is None or den <= 0:
        return None
    return num / den


def composite_mixed_iops(read_iops: float, write_iops: float, read_fraction: float = 0.7) -> float:
    if read_iops <= 0 or write_iops <= 0:
        raise ValueError('directional IOPS must be positive')
    if not 0.0 < read_fraction < 1.0:
        raise ValueError('read_fraction must be between 0 and 1')
    write_fraction = 1.0 - read_fraction
    return min(read_iops / read_fraction, write_iops / write_fraction)


def fio_p99_us(direction: dict[str, Any]) -> float | None:
    p = direction.get("clat_ns", {}).get("percentile", {})
    value = p.get("99.000000")
    return float(value) / 1000.0 if value is not None else None


def validate_fio_job(
    job: dict[str, Any], *, rw: str | None = None, rwmixread: int | None = None,
    bs_bytes: int | None = None, blockalign_bytes: int | None = None,
    ioengine: str | None = None, direct: int | None = None,
) -> None:
    if int(job.get("error", 0)) != 0:
        raise ValueError(f"fio error={job.get('error')}")
    options = job.get("job options") or {}
    expected: dict[str, str] = {}
    if rw is not None:
        expected["rw"] = rw
    if rwmixread is not None:
        expected["rwmixread"] = str(rwmixread)
    if bs_bytes is not None:
        expected["bs"] = str(bs_bytes)
    if blockalign_bytes is not None:
        expected["ba"] = str(blockalign_bytes)
    if ioengine is not None:
        expected["ioengine"] = ioengine
    if direct is not None:
        expected["direct"] = str(direct)
    mismatched = {k: (options.get(k), v) for k, v in expected.items() if options.get(k) != v}
    if mismatched:
        raise ValueError(f"fio job options mismatch: {mismatched}")


def load_storage_delta(path: Path) -> dict[str, Any]:
    raw = json.loads(path.read_text())
    before = raw.get("before") or {}
    after = raw.get("after") or {}
    if before.get("filesystem") != after.get("filesystem") or before.get("source") != after.get("source"):
        raise ValueError("storage source changed during case")
    before_devices = {d["name"]: d for d in before.get("devices", [])}
    after_devices = {d["name"]: d for d in after.get("devices", [])}
    if set(before_devices) != set(after_devices):
        raise ValueError(f"storage device set changed: {sorted(before_devices)} -> {sorted(after_devices)}")
    out: dict[str, Any] = {
        "storage_accounting_s": max(
            (float(after.get("captured_monotonic_ns", 0)) - float(before.get("captured_monotonic_ns", 0))) / 1e9,
            0.0,
        ),
        "storage_read_ios": 0,
        "storage_write_ios": 0,
        "storage_read_bytes": 0,
        "storage_write_bytes": 0,
        "storage_discard_bytes": 0,
        "storage_flushes": 0,
        "storage_io_ms": 0,
        "storage_weighted_io_ms": 0,
        "storage_devices": sorted(before_devices),
        "zfs_arc_hits": None,
        "zfs_arc_misses": None,
        "zfs_pool_arc_read_ios": None,
        "zfs_pool_arc_read_bytes": None,
        "zfs_pool_arc_write_ios": None,
        "zfs_pool_arc_write_bytes": None,
        "zfs_direct_read_ios": None,
        "zfs_direct_read_bytes": None,
        "zfs_direct_write_ios": None,
        "zfs_direct_write_bytes": None,
    }
    for name in before_devices:
        b = before_devices[name].get("stat", [])
        a = after_devices[name].get("stat", [])
        if len(b) < 11 or len(a) < 11:
            raise ValueError(f"short block stat for {name}")
        deltas = [int(y) - int(x) for x, y in zip(b, a)]
        # Linux block-stat field 9 (zero-based index 8) is I/Os currently in
        # progress: it is a gauge and may legitimately decrease. Validate only
        # monotonic counters that we consume below.
        monotonic_indexes = [0, 2, 4, 6, 9, 10]
        if len(deltas) >= 14:
            monotonic_indexes.append(13)
        if len(deltas) >= 16:
            monotonic_indexes.append(15)
        if any(deltas[index] < 0 for index in monotonic_indexes):
            raise ValueError(f"block counters moved backwards for {name}")
        # Linux block stat sector counters are expressed in 512-byte sectors.
        out["storage_read_ios"] += deltas[0]
        out["storage_read_bytes"] += deltas[2] * 512
        out["storage_write_ios"] += deltas[4]
        out["storage_write_bytes"] += deltas[6] * 512
        out["storage_io_ms"] += deltas[9]
        out["storage_weighted_io_ms"] += deltas[10]
        if len(deltas) >= 14:
            out["storage_discard_bytes"] += deltas[13] * 512
        if len(deltas) >= 16:
            out["storage_flushes"] += deltas[15]
    bpool = before.get("zfs_pool_io")
    apool = after.get("zfs_pool_io")
    if isinstance(bpool, dict) and isinstance(apool, dict):
        pool_fields = {
            "arc_read_count": "zfs_pool_arc_read_ios",
            "arc_read_bytes": "zfs_pool_arc_read_bytes",
            "arc_write_count": "zfs_pool_arc_write_ios",
            "arc_write_bytes": "zfs_pool_arc_write_bytes",
            "direct_read_count": "zfs_direct_read_ios",
            "direct_read_bytes": "zfs_direct_read_bytes",
            "direct_write_count": "zfs_direct_write_ios",
            "direct_write_bytes": "zfs_direct_write_bytes",
        }
        for source_name, output_name in pool_fields.items():
            delta = int(apool.get(source_name, 0)) - int(bpool.get(source_name, 0))
            if delta < 0:
                raise ValueError(f"ZFS pool counter moved backwards: {source_name}")
            out[output_name] = delta

    barc = before.get("zfs_arc")
    aarc = after.get("zfs_arc")
    if isinstance(barc, dict) and isinstance(aarc, dict):
        hits = int(aarc.get("hits", 0)) - int(barc.get("hits", 0))
        misses = int(aarc.get("misses", 0)) - int(barc.get("misses", 0))
        if hits >= 0 and misses >= 0:
            out["zfs_arc_hits"] = hits
            out["zfs_arc_misses"] = misses
    elapsed = max(float(out["storage_accounting_s"]), 1e-9)
    out["storage_read_mbps"] = float(out["storage_read_bytes"]) / elapsed / 1_000_000.0
    out["storage_write_mbps"] = float(out["storage_write_bytes"]) / elapsed / 1_000_000.0
    hits = out["zfs_arc_hits"]
    misses = out["zfs_arc_misses"]
    out["zfs_arc_hit_fraction"] = ratio(
        float(hits) if hits is not None else None,
        float(hits + misses) if hits is not None and misses is not None else None,
    )
    return out


def finite(value: Any) -> bool:
    try:
        return math.isfinite(float(value))
    except (TypeError, ValueError):
        return False
