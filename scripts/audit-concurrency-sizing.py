#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import hashlib
import json
import math
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable

SAFE_OP_SCALE_WORKLOADS = {"point-read", "range-scan", "read-heavy"}
STATE_CHANGING_WORKLOADS = {"balanced", "tiny-txn", "write-burst", "churn", "delete-burst"}
TARGET_SECONDS = 3.0
FLOOR_SECONDS = 2.0
SEVERE_SECONDS = 0.25
STATEFUL_MAX_TRIALS = 15
POLICY_VERSION = 1


def read_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text())


def read_cases(run_dir: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for path in sorted((run_dir / "cases").glob("*.json")):
        row = read_json(path)
        row["_case_file"] = path.name
        rows.append(row)
    return rows


def stock_ops(profile: str, workload: str, default_ops: int, records: int, clients: int) -> int:
    if workload == "tiny-txn":
        by_profile = {"smoke": 300, "quick": 1_000, "full": 2_500}
        ops = by_profile[profile]
    elif workload == "point-read" and profile == "quick":
        ops = 200_000
    elif workload == "range-scan":
        ops = 1_000 if profile == "quick" else max(1, default_ops // 100)
    else:
        ops = default_ops
    if workload == "delete-burst":
        ops = min(ops, records)
    if workload in {"range-scan", "tiny-txn"}:
        ops = max(ops, clients)
    return ops


def parse_jobs(run_dir: Path, support: dict[str, Any]) -> list[dict[str, Any]]:
    path = run_dir / "jobs.txt"
    if not path.is_file():
        raise ValueError(f"missing jobs.txt: {path}")
    profile = str(support["profile"])
    default_ops = int(support["default_ops"])
    records = int(support["records"])
    jobs: list[dict[str, Any]] = []
    for line_no, line in enumerate(path.read_text().splitlines(), 1):
        if not line.strip():
            continue
        fields = line.split("|")
        if len(fields) != 6:
            raise ValueError(f"invalid jobs.txt line {line_no}: {line!r}")
        scenario, engine, durability, workload, clients_raw, trial_raw = fields
        clients = int(clients_raw)
        trial = int(trial_raw)
        jobs.append(
            {
                "scenario": f"concurrency-{scenario}",
                "engine": engine,
                "durability": durability,
                "workload": workload,
                "clients": clients,
                "trial": trial,
                "records": records,
                "ops": stock_ops(profile, workload, default_ops, records, clients),
            }
        )
    return jobs


def client_key(item: dict[str, Any]) -> tuple[Any, ...]:
    return (
        item["scenario"],
        item["engine"],
        item["durability"],
        item["workload"],
        int(item["clients"]),
    )


def family_key(item: dict[str, Any]) -> tuple[Any, ...]:
    return client_key(item)[:-1]


def rounded_ops(value: float) -> int:
    if not math.isfinite(value) or value <= 0:
        raise ValueError(f"invalid operation recommendation: {value}")
    exponent = math.floor(math.log10(value))
    scale = 10**exponent
    normalized = value / scale
    for step in (1.0, 2.0, 5.0, 10.0):
        if normalized <= step:
            return max(1, int(step * scale))
    raise AssertionError("unreachable")


def _median(values: Iterable[float]) -> float:
    seq = list(values)
    if not seq:
        raise ValueError("median of empty sequence")
    return float(statistics.median(seq))


def audit(run_dir: Path, additional_runs: Iterable[Path] = ()) -> dict[str, Any]:
    support = read_json(run_dir / "support.json")
    if support.get("lane") != "kv-concurrency":
        raise ValueError(f"expected kv-concurrency support, got {support.get('lane')!r}")
    jobs = parse_jobs(run_dir, support)
    extra_runs = list(additional_runs)
    rows = read_cases(run_dir)
    for extra in extra_runs:
        rows.extend(read_cases(extra))
    seen_rows: set[tuple[Any, ...]] = set()
    for row in rows:
        identity = client_key(row) + (int(row["trial"]), int(row["records"]), int(row["ops_requested"]))
        if identity in seen_rows:
            raise ValueError(f"duplicate result identity across runs: {identity}")
        seen_rows.add(identity)

    planned_groups: dict[tuple[Any, ...], dict[str, Any]] = {}
    planned_families: dict[tuple[Any, ...], set[int]] = defaultdict(set)
    for job in jobs:
        ck = client_key(job)
        planned_groups.setdefault(ck, job)
        planned_families[family_key(job)].add(int(job["clients"]))

    observed: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        ck = client_key(row)
        if ck not in planned_groups:
            raise ValueError(f"result outside planned matrix: {row.get('_case_file')}: {ck}")
        expected_ops = int(planned_groups[ck]["ops"])
        if int(row["ops_requested"]) != expected_ops:
            raise ValueError(
                f"stock result op mismatch for {row.get('_case_file')}: "
                f"{row['ops_requested']} != {expected_ops}"
            )
        if int(row["ops_completed"]) != int(row["ops_requested"]):
            raise ValueError(f"partial result: {row.get('_case_file')}")
        elapsed = float(row["elapsed_s"])
        rate = float(row["ops_per_s"])
        if not math.isfinite(elapsed) or elapsed <= 0 or not math.isfinite(rate) or rate <= 0:
            raise ValueError(f"invalid timing: {row.get('_case_file')}")
        observed[ck].append(row)

    client_groups: list[dict[str, Any]] = []
    missing_probe_cases: list[dict[str, Any]] = []
    for ck, template in sorted(planned_groups.items()):
        samples = observed.get(ck, [])
        entry = dict(template)
        entry.pop("trial", None)
        entry["observed_trials"] = sorted(int(row["trial"]) for row in samples)
        entry["observed_count"] = len(samples)
        if samples:
            entry["median_elapsed_s"] = _median(float(row["elapsed_s"]) for row in samples)
            entry["median_ops_per_s"] = _median(float(row["ops_per_s"]) for row in samples)
            entry["min_elapsed_s"] = min(float(row["elapsed_s"]) for row in samples)
            entry["max_elapsed_s"] = max(float(row["elapsed_s"]) for row in samples)
        else:
            probe = dict(template)
            probe["trial"] = 1
            missing_probe_cases.append(probe)
        client_groups.append(entry)

    by_family_observed: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
    for item in client_groups:
        if item["observed_count"]:
            by_family_observed[family_key(item)].append(item)

    families: list[dict[str, Any]] = []
    counts: dict[str, int] = defaultdict(int)
    for fk, clients in sorted(planned_families.items()):
        scenario, engine, durability, workload = fk
        observed_groups = by_family_observed.get(fk, [])
        missing_clients = sorted(clients - {int(item["clients"]) for item in observed_groups})
        current_ops = stock_ops(
            str(support["profile"]), workload, int(support["default_ops"]), int(support["records"]), max(clients)
        )
        entry: dict[str, Any] = {
            "scenario": scenario,
            "engine": engine,
            "durability": durability,
            "workload": workload,
            "clients": sorted(clients),
            "missing_clients": missing_clients,
            "current_records": int(support["records"]),
            "current_ops": current_ops,
            "observed_client_groups": len(observed_groups),
            "planned_client_groups": len(clients),
            "semantics": "fixed-keyspace" if workload in SAFE_OP_SCALE_WORKLOADS else "state-changing",
        }
        if not observed_groups:
            entry["status"] = "needs-probe"
            entry["recommendation"] = "collect-stock-probe"
        else:
            fastest_elapsed = min(float(item["median_elapsed_s"]) for item in observed_groups)
            max_rate = max(float(item["median_ops_per_s"]) for item in observed_groups)
            entry["fastest_observed_median_elapsed_s"] = fastest_elapsed
            entry["max_observed_median_ops_per_s"] = max_rate
            if missing_clients:
                entry["status"] = "needs-probe"
                entry["recommendation"] = "collect-stock-probe"
            elif workload in SAFE_OP_SCALE_WORKLOADS:
                if fastest_elapsed < FLOOR_SECONDS:
                    suggested = rounded_ops(max_rate * TARGET_SECONDS)
                    entry["status"] = "undersized"
                    entry["recommendation"] = "more-ops"
                    entry["suggested_ops"] = max(current_ops, suggested)
                    entry["suggested_trials"] = int(support["trials"])
                else:
                    entry["status"] = "sized"
                    entry["recommendation"] = "keep-stock"
                    entry["suggested_ops"] = current_ops
                    entry["suggested_trials"] = int(support["trials"])
            else:
                needed_trials = max(int(support["trials"]), math.ceil(TARGET_SECONDS / fastest_elapsed))
                entry["suggested_ops"] = current_ops
                entry["suggested_trials"] = needed_trials
                if fastest_elapsed >= FLOOR_SECONDS:
                    entry["status"] = "sized"
                    entry["recommendation"] = "keep-stock"
                elif needed_trials <= STATEFUL_MAX_TRIALS and fastest_elapsed >= SEVERE_SECONDS:
                    entry["status"] = "undersized"
                    entry["recommendation"] = "more-fresh-trials"
                else:
                    entry["status"] = "redesign-required"
                    entry["recommendation"] = "state-preserving-longer-window"
        if observed_groups and workload in STATE_CHANGING_WORKLOADS:
            fastest = float(entry["fastest_observed_median_elapsed_s"])
            provisional_trials = max(int(support["trials"]), math.ceil(TARGET_SECONDS / fastest))
            entry["provisional_stateful_trials_needed"] = provisional_trials
            if fastest < SEVERE_SECONDS or provisional_trials > STATEFUL_MAX_TRIALS:
                entry["provisional_stateful_risk"] = "redesign-required"
            elif fastest < FLOOR_SECONDS:
                entry["provisional_stateful_risk"] = "more-fresh-trials"
            else:
                entry["provisional_stateful_risk"] = "sized"
        counts[entry["status"]] += 1
        families.append(entry)

    severe_stateful = sum(
        item.get("provisional_stateful_risk") == "redesign-required" for item in families
    )
    report = {
        "concurrency_sizing_policy_version": POLICY_VERSION,
        "run_dir": str(run_dir),
        "additional_run_dirs": [str(path) for path in extra_runs],
        "source_support": support,
        "thresholds": {
            "floor_seconds": FLOOR_SECONDS,
            "target_seconds": TARGET_SECONDS,
            "severe_seconds": SEVERE_SECONDS,
            "stateful_max_trials": STATEFUL_MAX_TRIALS,
            "safe_more_ops_workloads": sorted(SAFE_OP_SCALE_WORKLOADS),
            "state_changing_workloads": sorted(STATE_CHANGING_WORKLOADS),
        },
        "row_count": len(rows),
        "planned_case_count": len(jobs),
        "planned_client_group_count": len(planned_groups),
        "observed_client_group_count": len(observed),
        "planned_family_count": len(planned_families),
        "observed_family_count": len(by_family_observed),
        "missing_probe_case_count": len(missing_probe_cases),
        "observed_stateful_redesign_risk_count": severe_stateful,
        "counts": dict(sorted(counts.items())),
        "client_groups": client_groups,
        "families": families,
        "missing_probe_cases": missing_probe_cases,
    }
    return report


def markdown(report: dict[str, Any]) -> str:
    lines = [
        "# Concurrency sizing audit",
        "",
        f"Rows: **{report['row_count']}** / planned **{report['planned_case_count']}**; "
        f"client groups observed: **{report['observed_client_group_count']}** / **{report['planned_client_group_count']}**; "
        f"families observed: **{report['observed_family_count']}** / **{report['planned_family_count']}**.",
        "",
        "Policy: fixed-keyspace point-read/range-scan/read-heavy families may increase total ops, but the same total work is retained across client counts. State-changing append/churn/delete families never increase ops solely to lengthen a timing; moderately short families add fresh trials, while severe short families require a state-preserving longer-window design.",
        "",
        f"Observed state-changing families already severe enough to require redesign: **{report["observed_stateful_redesign_risk_count"]}**.",
        "",
        "## Family counts",
    ]
    for status, count in report["counts"].items():
        lines.append(f"- {status}: **{count}**")
    lines += ["", f"Missing one-shot stock probes: **{report['missing_probe_case_count']}**.", "", "## Families requiring redesign"]
    redesign = [f for f in report["families"] if f.get("provisional_stateful_risk") == "redesign-required"]
    if not redesign:
        lines.append("- none yet")
    else:
        for item in redesign:
            lines.append(
                f"- `{item['engine']}` {item['durability']} {item['workload']} ({item['scenario']}): "
                f"fastest observed median {item['fastest_observed_median_elapsed_s']:.6f}s; "
                f"stock ops {item['current_ops']}; naive fresh-trial requirement {item['provisional_stateful_trials_needed']}."
            )
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description="Audit KV concurrency quick sizing without changing stateful workload semantics")
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("--additional-run", type=Path, action="append", default=[])
    parser.add_argument("--json-out", type=Path)
    parser.add_argument("--markdown-out", type=Path)
    parser.add_argument("--probe-plan-out", type=Path)
    args = parser.parse_args()
    report = audit(args.run_dir, args.additional_run)
    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    if args.markdown_out:
        args.markdown_out.parent.mkdir(parents=True, exist_ok=True)
        args.markdown_out.write_text(markdown(report))
    if args.probe_plan_out:
        support_path = args.run_dir / "support.json"
        jobs_path = args.run_dir / "jobs.txt"
        plan = {
            "concurrency_probe_plan_version": 1,
            "kind": "stock-client-group-coverage",
            "source_run_dir": str(args.run_dir),
            "source_case_count": report["row_count"],
            "source_support_sha256": hashlib.sha256(support_path.read_bytes()).hexdigest(),
            "source_jobs_sha256": hashlib.sha256(jobs_path.read_bytes()).hexdigest(),
            "source_sizing_policy_version": POLICY_VERSION,
            "case_count": len(report["missing_probe_cases"]),
            "cases": report["missing_probe_cases"],
        }
        args.probe_plan_out.parent.mkdir(parents=True, exist_ok=True)
        args.probe_plan_out.write_text(json.dumps(plan, indent=2, sort_keys=True) + "\n")
    print(json.dumps({k: report[k] for k in ("row_count", "planned_case_count", "observed_client_group_count", "planned_client_group_count", "observed_family_count", "planned_family_count", "missing_probe_case_count", "observed_stateful_redesign_risk_count", "counts")}, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
