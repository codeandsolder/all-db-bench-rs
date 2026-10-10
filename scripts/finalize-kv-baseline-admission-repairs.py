#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import hashlib
import json
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any

DEFAULT_ADMISSION_POLICY = "pre-io+pre/post-external-v2"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def slug(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "-_" else "-" for ch in value)


def identity(item: dict[str, Any]) -> tuple[str, str, str]:
    return (str(item["engine"]), str(item["durability"]), str(item["workload"]))


def run_id(group: dict[str, Any], prefix: str) -> str:
    return (
        f"{prefix}-{slug(str(group['engine']))}-{slug(str(group['durability']))}-"
        f"{slug(str(group['workload']))}-e{int(group['suggested_effective_ops'])}-"
        f"t{int(group['suggested_trials'])}"
    )


def classify(rows: list[dict[str, Any]], thresholds: dict[str, Any]) -> dict[str, Any]:
    elapsed = [float(row["elapsed_s"]) for row in rows]
    rates = [float(row["ops_per_s"]) for row in rows]
    median_elapsed = statistics.median(elapsed)
    measured_total = sum(elapsed)
    median_rate = statistics.median(rates)
    cv = statistics.stdev(rates) / statistics.fmean(rates) if len(rates) > 1 and statistics.fmean(rates) else 0.0
    spread = (max(rates) - min(rates)) / median_rate if median_rate else float("inf")
    workload = str(rows[0]["workload"])
    read_only = workload in set(thresholds["read_only_workloads"])
    undersized = (
        median_elapsed < float(thresholds["read_only_min_seconds"])
        if read_only
        else measured_total < float(thresholds["stateful_min_total_seconds"])
    )
    variable = cv > float(thresholds["max_cv"]) or spread > float(thresholds["max_relative_spread"])
    status = "undersized" if undersized else ("variable" if variable else "accepted")
    return {
        "final_status": status,
        "median_elapsed_s": median_elapsed,
        "measured_total_s": measured_total,
        "throughput_cv": cv,
        "throughput_relative_spread": spread,
    }


def finalize(
    *,
    base_rows: list[dict[str, Any]],
    base_manifest: dict[str, Any],
    base_manifest_sha256: str,
    base_results_sha256: str,
    plan: dict[str, Any],
    plan_sha256: str,
    repair_repo: Path,
    run_prefix: str,
    thresholds: dict[str, Any],
    expected_admission_policy: str,
    expected_bench_sha256: str,
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    if plan.get("admission_repair_policy_version") != 1:
        raise ValueError("unsupported admission repair plan")
    if plan.get("source_manifest_sha256") != base_manifest_sha256:
        raise ValueError("repair plan source manifest hash does not match base manifest")
    if not base_manifest.get("complete"):
        raise ValueError("base selection is incomplete")

    grouped_base: dict[tuple[str, str, str], list[dict[str, Any]]] = defaultdict(list)
    for row in base_rows:
        grouped_base[identity(row)].append(row)

    sources = [dict(source) for source in base_manifest.get("sources", [])]
    source_index: dict[tuple[str, str, str], int] = {}
    for index, source in enumerate(sources):
        key = identity(source)
        if key in source_index:
            raise ValueError(f"duplicate base manifest source identity: {key}")
        source_index[key] = index

    repairs: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
    repair_records: list[dict[str, Any]] = []
    support_hashes: set[str] = set()
    noise_hashes: set[str] = set()
    runner_hashes: set[str] = set()

    for group in plan.get("groups", []):
        key = identity(group)
        if key in repairs:
            raise ValueError(f"duplicate repair identity: {key}")
        if key not in grouped_base or key not in source_index:
            raise ValueError(f"repair identity absent from base selection: {key}")

        expected_trials = int(group["suggested_trials"])
        expected_ops = int(group["suggested_effective_ops"])
        expected_records = int(group["records"])
        if len(grouped_base[key]) != expected_trials:
            raise ValueError(
                f"base row count for {key} is {len(grouped_base[key])}, expected {expected_trials}"
            )

        rid = run_id(group, run_prefix)
        run_dir = repair_repo / "results" / "runs" / rid
        results_path = run_dir / "results.ndjson"
        support_path = run_dir / "support.json"
        summary_path = run_dir / "summary.json"
        if not results_path.is_file() or not support_path.is_file() or not summary_path.is_file():
            raise ValueError(f"incomplete repair run: {rid}")

        support = json.loads(support_path.read_text())
        if support.get("admission_policy") != expected_admission_policy:
            raise ValueError(f"wrong admission policy for {rid}: {support.get('admission_policy')!r}")
        if support.get("benchmark_binary_sha256") != expected_bench_sha256:
            raise ValueError(f"wrong benchmark binary for {rid}")
        if support.get("lane") != "kv" or support.get("profile") != "quick":
            raise ValueError(f"wrong support lane/profile for {rid}")
        if int(support.get("trials", -1)) != expected_trials or int(support.get("records", -1)) != expected_records:
            raise ValueError(f"support geometry mismatch for {rid}")
        support_hashes.add(sha256(support_path))
        noise_hash = str(support.get("noise_guard_sha256") or "")
        runner_hash = str(support.get("runner_sha256") or "")
        if not noise_hash or not runner_hash:
            raise ValueError(f"missing runner/noise provenance for {rid}")
        noise_hashes.add(noise_hash)
        runner_hashes.add(runner_hash)

        summary = json.loads(summary_path.read_text())
        if summary.get("row_count") != expected_trials or summary.get("group_count") != 1 or summary.get("problems"):
            raise ValueError(f"repair summary invalid for {rid}")

        rows = [json.loads(line) for line in results_path.read_text().splitlines() if line.strip()]
        if len(rows) != expected_trials:
            raise ValueError(f"repair row count mismatch for {rid}")
        trials = sorted(int(row["trial"]) for row in rows)
        if trials != list(range(1, expected_trials + 1)):
            raise ValueError(f"repair trial sequence mismatch for {rid}: {trials}")
        for row in rows:
            if identity(row) != key:
                raise ValueError(f"repair identity mismatch in {rid}")
            if int(row["ops_requested"]) != expected_ops or int(row["records"]) != expected_records:
                raise ValueError(f"repair row geometry mismatch in {rid}")

        quality = classify(rows, thresholds)
        if quality["final_status"] == "undersized":
            raise ValueError(f"fresh repair remains undersized: {rid}")

        repairs[key] = rows
        old_source = sources[source_index[key]]
        if int(old_source.get("rejected_pressure_attempts", 0)) <= 0:
            raise ValueError(f"repair target had no legacy rejected attempts: {key}")
        new_source = {
            **old_source,
            **quality,
            "source": "admission-v2-repair",
            "run_id": rid,
            "ops_requested": expected_ops,
            "trials": expected_trials,
            "admission_policy": expected_admission_policy,
            "support_sha256": sha256(support_path),
            "noise_guard_sha256": noise_hash,
            "runner_sha256": runner_hash,
            "legacy_run_id": group["legacy_run_id"],
            "legacy_rejected_pressure_attempts": int(group["legacy_rejected_pressure_attempts"]),
            "rejected_pressure_attempts": 0,
        }
        sources[source_index[key]] = new_source
        repair_records.append(
            {
                "engine": key[0],
                "durability": key[1],
                "workload": key[2],
                "run_id": rid,
                "legacy_run_id": group["legacy_run_id"],
                "legacy_rejected_pressure_attempts": int(group["legacy_rejected_pressure_attempts"]),
                **quality,
                "support_sha256": sha256(support_path),
                "noise_guard_sha256": noise_hash,
                "runner_sha256": runner_hash,
            }
        )

    expected_repairs = int(plan.get("suspect_source_count", -1))
    if len(repairs) != expected_repairs:
        raise ValueError(f"repair count mismatch: {len(repairs)} != {expected_repairs}")
    if len(noise_hashes) != 1 or len(runner_hashes) != 1:
        raise ValueError(
            f"repair runs do not share one runner/noise identity: runners={runner_hashes}, noise={noise_hashes}"
        )

    final_rows: list[dict[str, Any]] = []
    for key, rows in grouped_base.items():
        final_rows.extend(repairs.get(key, rows))
    final_rows.sort(
        key=lambda row: (
            str(row["engine"]),
            str(row["durability"]),
            str(row["workload"]),
            int(row["trial"]),
        )
    )
    if len(final_rows) != len(base_rows):
        raise ValueError(f"row count changed during repair: {len(base_rows)} -> {len(final_rows)}")

    status_counts: dict[str, int] = defaultdict(int)
    for source in sources:
        status_counts[str(source["final_status"])] += 1

    manifest = {
        **base_manifest,
        "selection_version": 5,
        "admission_repair_policy_version": 1,
        "admission_repair_complete": True,
        "base_selected_results_sha256": base_results_sha256,
        "base_selected_manifest_sha256": base_manifest_sha256,
        "repair_plan_sha256": plan_sha256,
        "repair_run_prefix": run_prefix,
        "repair_admission_policy": expected_admission_policy,
        "repair_benchmark_binary_sha256": expected_bench_sha256,
        "repair_noise_guard_sha256": next(iter(noise_hashes), None),
        "repair_runner_sha256": next(iter(runner_hashes), None),
        "repair_support_sha256": sorted(support_hashes),
        "repaired_groups": repair_records,
        "selected_followup_rejected_pressure_attempts": sum(
            int(source.get("rejected_pressure_attempts", 0)) for source in sources
        ),
        "legacy_repaired_rejected_pressure_attempts": sum(
            int(item["legacy_rejected_pressure_attempts"]) for item in repair_records
        ),
        "final_status_counts": dict(sorted(status_counts.items())),
        "selected_rows": len(final_rows),
        "selected_groups": len(sources),
        "sources": sources,
    }
    return final_rows, manifest


def main() -> int:
    parser = argparse.ArgumentParser(description="Replace legacy pressure-selected KV baseline groups with fresh admission-v2 measurements")
    parser.add_argument("--base-selected-dir", type=Path, required=True)
    parser.add_argument("--repair-plan", type=Path, required=True)
    parser.add_argument("--repair-repo", type=Path, required=True)
    parser.add_argument("--run-prefix", required=True)
    parser.add_argument("--audit", type=Path, required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--expected-admission-policy", default=DEFAULT_ADMISSION_POLICY)
    parser.add_argument("--expected-bench-sha256", required=True)
    args = parser.parse_args()

    base_results = args.base_selected_dir / "results.ndjson"
    base_manifest_path = args.base_selected_dir / "manifest.json"
    base_rows = [json.loads(line) for line in base_results.read_text().splitlines() if line.strip()]
    base_manifest = json.loads(base_manifest_path.read_text())
    plan = json.loads(args.repair_plan.read_text())
    audit = json.loads(args.audit.read_text())
    thresholds = audit["thresholds"]

    final_rows, manifest = finalize(
        base_rows=base_rows,
        base_manifest=base_manifest,
        base_manifest_sha256=sha256(base_manifest_path),
        base_results_sha256=sha256(base_results),
        plan=plan,
        plan_sha256=sha256(args.repair_plan),
        repair_repo=args.repair_repo,
        run_prefix=args.run_prefix,
        thresholds=thresholds,
        expected_admission_policy=args.expected_admission_policy,
        expected_bench_sha256=args.expected_bench_sha256,
    )
    args.out_dir.mkdir(parents=True, exist_ok=True)
    results_out = args.out_dir / "results.ndjson"
    manifest_out = args.out_dir / "manifest.json"
    results_out.write_text("".join(json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n" for row in final_rows))
    manifest_out.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(
        f"rows={len(final_rows)} groups={manifest['selected_groups']} "
        f"repaired={len(manifest['repaired_groups'])} "
        f"status={manifest['final_status_counts']} out={args.out_dir}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
