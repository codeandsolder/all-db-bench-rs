#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any

IDENTITY_FIELDS = (
    "lane",
    "scenario",
    "engine",
    "engine_version",
    "durability",
    "workload",
    "read_materialization",
    "write_materialization",
    "records",
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
    "configuration",
)
AUDIT_FIELDS = ("engine", "engine_version", "durability", "workload", "read_materialization", "write_materialization", "records", "ops_requested")


def slug(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "-_" else "-" for ch in value)


def run_id(group: dict[str, Any], resize_prefix: str) -> str:
    return (
        f"{resize_prefix}-"
        f"{slug(str(group['engine']))}-{slug(str(group['durability']))}-"
        f"{slug(str(group['workload']))}-e{int(group['suggested_effective_ops'])}"
        f"-t{int(group['suggested_trials'])}"
    )


def _identity_value(row: dict[str, Any], field: str) -> Any:
    if field == "read_materialization":
        return row.get("read_materialization", "legacy-read-v0")
    if field == "write_materialization":
        return row.get("write_materialization", "legacy-return-v0")
    return row.get(field)


def semantic_manifest_fields(row: dict[str, Any]) -> dict[str, str]:
    if row.get("lane") != "record":
        return {}
    return {
        "read_materialization": str(row.get("read_materialization", "legacy-read-v0")),
        "write_materialization": str(row.get("write_materialization", "legacy-return-v0")),
    }


def identity(row: dict[str, Any]) -> tuple[Any, ...]:
    return tuple(_identity_value(row, field) for field in IDENTITY_FIELDS)


def audit_key(row: dict[str, Any]) -> tuple[Any, ...]:
    return tuple(_identity_value(row, field) for field in AUDIT_FIELDS)


def read_ndjson(path: Path) -> list[dict[str, Any]]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def rejected_pressure_attempts(run_dir: Path) -> int:
    root = run_dir / "rejected-pressure"
    return sum(1 for _ in root.glob("*/attempt-*/rejection.json")) if root.is_dir() else 0


def validate_trials(rows: list[dict[str, Any]], *, label: str, expected_count: int) -> None:
    trials = sorted(int(row["trial"]) for row in rows)
    expected = list(range(1, expected_count + 1))
    if trials != expected:
        raise ValueError(f"{label}: expected trials {expected}, got {trials}")



def final_quality(rows: list[dict[str, Any]], thresholds: dict[str, Any]) -> dict[str, Any]:
    elapsed = [float(row["elapsed_s"]) for row in rows]
    rates = [float(row["ops_per_s"]) for row in rows]
    median_elapsed = statistics.median(elapsed)
    total_elapsed = sum(elapsed)
    median_rate = statistics.median(rates)
    mean_rate = statistics.fmean(rates)
    throughput_cv = statistics.stdev(rates) / mean_rate if len(rates) > 1 and mean_rate else 0.0
    relative_spread = (max(rates) - min(rates)) / median_rate if median_rate else float("inf")
    workload = str(rows[0].get("workload"))
    read_only = workload in set(thresholds["read_only_workloads"])
    undersized = (
        median_elapsed < float(thresholds["read_only_min_seconds"])
        if read_only
        else total_elapsed < float(thresholds["stateful_min_total_seconds"])
    )
    variable = (
        throughput_cv > float(thresholds["max_cv"])
        or relative_spread > float(thresholds["max_relative_spread"])
    )
    status = "undersized" if undersized else "variable" if variable else "accepted"
    return {
        "final_status": status,
        "median_elapsed_s": median_elapsed,
        "total_elapsed_s": total_elapsed,
        "throughput_cv": throughput_cv,
        "throughput_relative_spread": relative_spread,
    }


def select_rows(
    stock_rows: list[dict[str, Any]],
    audit: dict[str, Any],
    resize_root: Path,
    *,
    resize_prefix: str = "20261006-kv-resize-v4",
    allow_missing_resize: bool,
    quality_repair_plan: dict[str, Any] | None = None,
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    policy_version = int(audit.get("sizing_policy_version", -1))
    if policy_version not in {2, 3}:
        raise ValueError(f"unsupported sizing policy: {audit.get('sizing_policy_version')!r}")
    if policy_version >= 3:
        missing_semantics = [
            row for row in stock_rows
            if row.get("lane") == "record"
            and (not row.get("read_materialization") or not row.get("write_materialization"))
        ]
        if missing_semantics:
            raise ValueError(
                "sizing policy v3 record rows require explicit read/write materialization"
            )
    thresholds = audit.get("thresholds", {})
    stock_trial_count = int(thresholds.get("expect_trials", 3))

    stock_groups: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
    for row in stock_rows:
        stock_groups[identity(row)].append(row)

    audit_groups = {audit_key(group): group for group in audit["groups"]}
    quality_groups: dict[tuple[Any, ...], dict[str, Any]] = {}
    if quality_repair_plan is not None:
        if quality_repair_plan.get("quality_policy_version") != 1:
            raise ValueError(
                f"unsupported stock quality policy: {quality_repair_plan.get('quality_policy_version')!r}"
            )
        quality_groups = {audit_key(group): group for group in quality_repair_plan.get("groups", [])}
    output: list[dict[str, Any]] = []
    selected_stock = 0
    selected_resize = 0
    selected_quality_repair = 0
    pending: list[str] = []
    sources: list[dict[str, Any]] = []

    for group_identity, rows in sorted(stock_groups.items(), key=lambda item: tuple(str(v) for v in item[0])):
        validate_trials(
            rows,
            label=f"stock {rows[0].get('engine')}/{rows[0].get('durability')}/{rows[0].get('workload')}",
            expected_count=stock_trial_count,
        )
        first = rows[0]
        key = audit_key(first)
        group = audit_groups.get(key)
        if group is None:
            raise ValueError(f"stock group missing from audit: {key}")

        quality_group = quality_groups.get(key)
        if quality_group is not None:
            rid = run_id(quality_group, resize_prefix)
            results_path = resize_root / rid / "results.ndjson"
            summary_path = resize_root / rid / "summary.json"
            if not results_path.is_file() or not summary_path.is_file():
                pending.append(rid)
                if allow_missing_resize:
                    continue
                raise ValueError(f"missing quality-repair result: {rid}")
            expected_trials = int(quality_group["suggested_trials"])
            summary = json.loads(summary_path.read_text())
            if summary.get("row_count") != expected_trials or summary.get("group_count") != 1 or summary.get("problems"):
                raise ValueError(f"invalid quality-repair summary: {rid}")
            repaired = read_ndjson(results_path)
            validate_trials(repaired, label=f"quality repair {rid}", expected_count=expected_trials)
            if len({identity(row) for row in repaired}) != 1 or identity(repaired[0]) != group_identity:
                raise ValueError(f"quality-repair identity differs from stock group: {rid}")
            expected_ops = int(quality_group["suggested_effective_ops"])
            actual_ops = {int(row["ops_requested"]) for row in repaired}
            if actual_ops != {expected_ops}:
                raise ValueError(
                    f"quality-repair op count mismatch for {rid}: expected {expected_ops}, got {sorted(actual_ops)}"
                )
            quality = final_quality(repaired, thresholds)
            if quality["final_status"] == "undersized":
                raise ValueError(f"quality-repair result remains undersized: {rid}")
            output.extend(repaired)
            selected_quality_repair += 1
            sources.append(
                {
                    "engine": first["engine"],
                    "durability": first["durability"],
                    "workload": first["workload"],
                    **semantic_manifest_fields(first),
                    "source": "quality-repair",
                    "resize_strategy": "quality-repair",
                    "run_id": rid,
                    "ops_requested": expected_ops,
                    "trials": expected_trials,
                    "rejected_pressure_attempts": rejected_pressure_attempts(resize_root / rid),
                    **quality,
                }
            )
            continue

        if group["status"] != "undersized":
            quality = final_quality(rows, thresholds)
            if quality["final_status"] != group["status"]:
                raise ValueError(
                    f"stock quality no longer matches audit for {key}: audit={group['status']} recomputed={quality['final_status']}"
                )
            output.extend(rows)
            selected_stock += 1
            sources.append(
                {
                    "engine": first["engine"],
                    "durability": first["durability"],
                    "workload": first["workload"],
                    **semantic_manifest_fields(first),
                    "source": "stock",
                    "ops_requested": int(first["ops_requested"]),
                    "trials": stock_trial_count,
                    **quality,
                }
            )
            continue

        rid = run_id(group, resize_prefix)
        results_path = resize_root / rid / "results.ndjson"
        summary_path = resize_root / rid / "summary.json"
        if not results_path.is_file() or not summary_path.is_file():
            pending.append(rid)
            if allow_missing_resize:
                continue
            raise ValueError(f"missing resized result: {rid}")

        expected_resize_trials = int(group["suggested_trials"])
        summary = json.loads(summary_path.read_text())
        if (
            summary.get("row_count") != expected_resize_trials
            or summary.get("group_count") != 1
            or summary.get("problems")
        ):
            raise ValueError(f"invalid resized summary: {rid}")
        resized = read_ndjson(results_path)
        validate_trials(resized, label=f"resize {rid}", expected_count=expected_resize_trials)
        if len({identity(row) for row in resized}) != 1 or identity(resized[0]) != group_identity:
            raise ValueError(f"resized identity differs from stock group: {rid}")
        expected_ops = int(group["suggested_effective_ops"])
        actual_ops = {int(row["ops_requested"]) for row in resized}
        if actual_ops != {expected_ops}:
            raise ValueError(f"resized op count mismatch for {rid}: expected {expected_ops}, got {sorted(actual_ops)}")
        quality = final_quality(resized, thresholds)
        if quality["final_status"] == "undersized":
            raise ValueError(f"resized result remains undersized: {rid}")

        output.extend(resized)
        selected_resize += 1
        sources.append(
            {
                "engine": first["engine"],
                "durability": first["durability"],
                "workload": first["workload"],
                **semantic_manifest_fields(first),
                "source": "resize",
                "resize_strategy": group.get("resize_strategy"),
                "run_id": rid,
                "ops_requested": expected_ops,
                "trials": expected_resize_trials,
                "rejected_pressure_attempts": rejected_pressure_attempts(resize_root / rid),
                **quality,
            }
        )

    expected_audit_groups = int(audit.get("group_count", len(audit_groups)))
    if len(stock_groups) != expected_audit_groups:
        raise ValueError(
            f"stock/audit group count mismatch: stock={len(stock_groups)} audit={expected_audit_groups}"
        )

    status_counts = {
        status: sum(source["final_status"] == status for source in sources)
        for status in ("accepted", "variable", "undersized")
    }
    manifest = {
        "sizing_policy_version": audit["sizing_policy_version"],
        "stock_rows": len(stock_rows),
        "stock_groups": len(stock_groups),
        "selected_rows": len(output),
        "selected_groups": selected_stock + selected_resize + selected_quality_repair,
        "selected_stock_groups": selected_stock,
        "selected_resize_groups": selected_resize,
        "selected_quality_repair_groups": selected_quality_repair,
        "pending_resize_groups": len(pending),
        "pending_run_ids": pending,
        "complete": not pending,
        "final_status_counts": status_counts,
        "selected_followup_rejected_pressure_attempts": sum(
            int(source.get("rejected_pressure_attempts", 0)) for source in sources
        ),
        "sources": sources,
    }
    return output, manifest


def main() -> int:
    parser = argparse.ArgumentParser(description="Select stock or resized baseline groups without mixing semantics")
    parser.add_argument("stock_results", type=Path)
    parser.add_argument("audit", type=Path)
    parser.add_argument("resize_root", type=Path)
    parser.add_argument("--ndjson-out", type=Path, required=True)
    parser.add_argument("--manifest-out", type=Path, required=True)
    parser.add_argument("--resize-prefix", default="20261006-kv-resize-v4")
    parser.add_argument("--quality-repair-plan", type=Path)
    parser.add_argument("--allow-missing-resize", action="store_true")
    args = parser.parse_args()

    rows, manifest = select_rows(
        read_ndjson(args.stock_results),
        json.loads(args.audit.read_text()),
        args.resize_root,
        resize_prefix=args.resize_prefix,
        allow_missing_resize=args.allow_missing_resize,
        quality_repair_plan=(json.loads(args.quality_repair_plan.read_text()) if args.quality_repair_plan else None),
    )
    manifest["stock_rejected_pressure_attempts"] = rejected_pressure_attempts(args.stock_results.parent)
    args.ndjson_out.parent.mkdir(parents=True, exist_ok=True)
    args.ndjson_out.write_text("".join(json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n" for row in rows))
    args.manifest_out.parent.mkdir(parents=True, exist_ok=True)
    args.manifest_out.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(
        json.dumps(
            {
                key: manifest[key]
                for key in (
                    "selected_rows",
                    "selected_groups",
                    "selected_stock_groups",
                    "selected_resize_groups",
                    "selected_quality_repair_groups",
                    "pending_resize_groups",
                    "complete",
                )
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
