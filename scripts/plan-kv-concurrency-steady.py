#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import hashlib
import json
import math
from pathlib import Path
from typing import Any, Iterable

from concurrency_support_provenance import canonical_identity_sha256, support_identity

FIXED_WORKLOADS = {"point-read", "range-scan", "read-heavy"}
BOUNDED_WORKLOADS = {"balanced", "tiny-txn", "write-burst", "churn"}
DELETE_WORKLOAD = "delete-burst"
PLAN_VERSION = 2
DEFAULT_BOUNDED_CHURN_SLOTS = 8192
DEFAULT_FINAL_TRIALS = 3


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def case_set_sha256(paths: Iterable[Path]) -> str:
    digest = hashlib.sha256()
    for path in sorted(paths, key=lambda p: p.name):
        digest.update(path.name.encode())
        digest.update(b"\0")
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


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


def family_key(item: dict[str, Any]) -> tuple[str, str, str, str]:
    return (str(item["scenario"]), str(item["engine"]), str(item["durability"]), str(item["workload"]))


def load_complete_audit(path: Path) -> dict[str, Any]:
    audit = json.loads(path.read_text())
    if audit.get("concurrency_sizing_policy_version") != 1:
        raise ValueError(f"unsupported sizing audit version: {audit.get("concurrency_sizing_policy_version")!r}")
    if int(audit.get("missing_probe_case_count", -1)) != 0:
        raise ValueError(f"sizing audit still has {audit.get("missing_probe_case_count")} missing client groups")
    families = audit.get("families")
    if not isinstance(families, list) or len(families) != int(audit.get("planned_family_count", -1)):
        raise ValueError("sizing audit family count mismatch")
    pending = [family_key(f) for f in families if f.get("status") == "needs-probe" or f.get("missing_clients")]
    if pending:
        raise ValueError(f"sizing audit contains incomplete families: {pending[:5]}")
    if not isinstance(audit.get("measurement_identity"), dict):
        raise ValueError("sizing audit is missing measurement identity")
    return audit


def target_seconds(audit: dict[str, Any]) -> float:
    value = float(audit["thresholds"]["target_seconds"])
    if not math.isfinite(value) or value <= 0:
        raise ValueError(f"invalid target_seconds={value}")
    return value


def estimated_ops(family: dict[str, Any], target_s: float) -> int:
    rate = float(family["max_observed_median_ops_per_s"])
    current = int(family["current_ops"])
    return max(current, rounded_ops(rate * target_s))


def case_template(
    family: dict[str, Any], *, ops: int, clients: int, trial: int, state_evolution: str,
    write_pattern: str, bounded_churn_slots: int,
) -> dict[str, Any]:
    return {
        "scenario": family["scenario"],
        "engine": family["engine"],
        "durability": family["durability"],
        "workload": family["workload"],
        "clients": int(clients),
        "records": int(family["current_records"]),
        "ops": int(ops),
        "trial": int(trial),
        "state_evolution": state_evolution,
        "write_pattern": write_pattern,
        "bounded_churn_slots": int(bounded_churn_slots),
    }


def build_calibration_plan(audit_path: Path) -> dict[str, Any]:
    audit = load_complete_audit(audit_path)
    target_s = target_seconds(audit)
    cases: list[dict[str, Any]] = []
    calibrated_families: list[dict[str, Any]] = []
    for family in audit["families"]:
        workload = str(family["workload"])
        if workload in FIXED_WORKLOADS:
            if family["status"] != "undersized":
                continue
            ops = int(family["suggested_ops"])
            state = "growth"
            write_pattern = "append"
            slots = 0
            reason = "fixed-keyspace-resize"
        elif workload in BOUNDED_WORKLOADS:
            ops = estimated_ops(family, target_s)
            state = "bounded"
            write_pattern = "update-uniform"
            slots = DEFAULT_BOUNDED_CHURN_SLOTS
            reason = "bounded-steady-state"
        elif workload == DELETE_WORKLOAD:
            continue
        else:
            raise ValueError(f"unclassified workload: {workload}")
        calibrated_families.append({
            "scenario": family["scenario"], "engine": family["engine"], "durability": family["durability"],
            "workload": workload, "reason": reason, "stock_ops": int(family["current_ops"]), "probe_ops": ops,
            "clients": [int(v) for v in family["clients"]], "state_evolution": state,
            "write_pattern": write_pattern, "bounded_churn_slots": slots,
        })
        for clients in family["clients"]:
            cases.append(case_template(
                family, ops=ops, clients=int(clients), trial=1, state_evolution=state,
                write_pattern=write_pattern, bounded_churn_slots=slots,
            ))
    plan = {
        "concurrency_probe_plan_version": PLAN_VERSION,
        "kind": "steady-sizing-calibration",
        "expect_trials": 1,
        "source_sizing_audit_sha256": sha256(audit_path),
        "source_sizing_policy_version": audit["concurrency_sizing_policy_version"],
        "expected_measurement_identity": audit["measurement_identity"],
        "expected_measurement_identity_sha256": canonical_identity_sha256(audit["measurement_identity"]),
        "target_seconds": target_s,
        "bounded_churn_slots": DEFAULT_BOUNDED_CHURN_SLOTS,
        "family_count": len(calibrated_families),
        "families": calibrated_families,
        "case_count": len(cases),
        "cases": cases,
    }
    return plan


def _result_identity(item: dict[str, Any]) -> tuple[Any, ...]:
    return (
        item["scenario"], item["engine"], item["durability"], item["workload"], int(item["clients"]),
        int(item["records"]), int(item.get("ops", item.get("ops_requested"))), int(item["trial"]),
        item.get("state_evolution", "growth"), item.get("write_pattern", "append"),
        int(item.get("bounded_churn_slots", 0)),
    )


def load_calibration_results(plan_path: Path, run_dir: Path) -> tuple[dict[tuple[Any, ...], dict[str, Any]], dict[str, str]]:
    plan = json.loads(plan_path.read_text())
    if plan.get("concurrency_probe_plan_version") != PLAN_VERSION or plan.get("kind") != "steady-sizing-calibration":
        raise ValueError("not a steady-sizing calibration plan")
    support_path = run_dir / "support.json"
    if not support_path.is_file():
        raise ValueError(f"missing calibration support: {support_path}")
    support = json.loads(support_path.read_text())
    plan_sha = sha256(plan_path)
    if support.get("plan_sha256") != plan_sha:
        raise ValueError(f"calibration support plan SHA mismatch: {support.get('plan_sha256')} != {plan_sha}")
    expected_identity = plan.get("expected_measurement_identity")
    if not isinstance(expected_identity, dict):
        raise ValueError("calibration plan is missing measurement identity")
    actual_identity = support_identity(
        support,
        expected_lane="kv-concurrency",
        repo=Path(__file__).resolve().parents[1],
        runner_name="run-kv-concurrency-plan.sh",
        label=str(support_path),
    )
    if actual_identity != expected_identity:
        raise ValueError(
            "calibration measurement identity differs from sizing source: "
            f"{canonical_identity_sha256(actual_identity)} != {canonical_identity_sha256(expected_identity)}"
        )
    case_paths = sorted((run_dir / "cases").glob("*.json"))
    if len(case_paths) != int(plan["case_count"]):
        raise ValueError(f"calibration incomplete: {len(case_paths)} / {plan["case_count"]} cases")
    planned = {_result_identity(item): item for item in plan["cases"]}
    if len(planned) != len(plan["cases"]):
        raise ValueError("duplicate calibration plan identity")
    results: dict[tuple[Any, ...], dict[str, Any]] = {}
    for path in case_paths:
        row = json.loads(path.read_text())
        identity = _result_identity(row)
        expected = planned.get(identity)
        if expected is None:
            raise ValueError(f"calibration result outside plan: {path.name}: {identity}")
        if identity in results:
            raise ValueError(f"duplicate calibration result: {identity}")
        if int(row.get("ops_completed", -1)) != int(row.get("ops_requested", -2)):
            raise ValueError(f"partial calibration result: {path.name}")
        elapsed = float(row["elapsed_s"])
        rate = float(row["ops_per_s"])
        if not math.isfinite(elapsed) or elapsed <= 0 or not math.isfinite(rate) or rate <= 0:
            raise ValueError(f"invalid calibration timing: {path.name}")
        results[identity] = row
    missing = set(planned) - set(results)
    if missing:
        raise ValueError(f"missing calibration results: {list(missing)[:3]}")
    provenance = {
        "calibration_plan_sha256": plan_sha,
        "calibration_support_sha256": sha256(support_path),
        "calibration_case_set_sha256": case_set_sha256(case_paths),
        "expected_measurement_identity": expected_identity,
        "expected_measurement_identity_sha256": canonical_identity_sha256(expected_identity),
    }
    return results, provenance


def _calibrated_family_ops(
    family: dict[str, Any], plan: dict[str, Any], results: dict[tuple[Any, ...], dict[str, Any]], target_s: float,
) -> int:
    candidates = [item for item in plan["cases"] if family_key(item) == family_key(family)]
    if not candidates:
        raise ValueError(f"family missing from calibration plan: {family_key(family)}")
    rates = [float(results[_result_identity(item)]["ops_per_s"]) for item in candidates]
    return max(int(family["current_ops"]), rounded_ops(max(rates) * target_s))


def build_final_plans(
    audit_path: Path, calibration_plan_path: Path, calibration_run_dir: Path, *, final_trials: int = DEFAULT_FINAL_TRIALS,
) -> tuple[dict[str, Any], dict[str, Any]]:
    if final_trials < 1:
        raise ValueError("final_trials must be positive")
    audit = load_complete_audit(audit_path)
    target_s = target_seconds(audit)
    calibration_plan = json.loads(calibration_plan_path.read_text())
    if calibration_plan.get("source_sizing_audit_sha256") != sha256(audit_path):
        raise ValueError("calibration plan was generated from a different sizing audit")
    results, provenance = load_calibration_results(calibration_plan_path, calibration_run_dir)
    final_cases: list[dict[str, Any]] = []
    delete_cases: list[dict[str, Any]] = []
    selected: list[dict[str, Any]] = []
    delete_families: list[dict[str, Any]] = []
    for family in audit["families"]:
        workload = str(family["workload"])
        if workload == DELETE_WORKLOAD:
            ops = int(family["current_ops"])
            delete_families.append({**{k: family[k] for k in ("scenario","engine","durability","workload")}, "ops": ops})
            for trial in range(1, final_trials + 1):
                for clients in family["clients"]:
                    delete_cases.append(case_template(
                        family, ops=ops, clients=int(clients), trial=trial, state_evolution="growth",
                        write_pattern="append", bounded_churn_slots=0,
                    ))
            continue
        if workload in FIXED_WORKLOADS:
            if family["status"] == "undersized":
                ops = _calibrated_family_ops(family, calibration_plan, results, target_s)
                source = "fixed-calibration"
            else:
                ops = int(family["current_ops"])
                source = "stock-sized"
            state = "growth"; write_pattern = "append"; slots = 0
        elif workload in BOUNDED_WORKLOADS:
            ops = _calibrated_family_ops(family, calibration_plan, results, target_s)
            source = "bounded-calibration"
            state = "bounded"; write_pattern = "update-uniform"; slots = DEFAULT_BOUNDED_CHURN_SLOTS
        else:
            raise ValueError(f"unclassified workload: {workload}")
        selected.append({
            "scenario": family["scenario"], "engine": family["engine"], "durability": family["durability"],
            "workload": workload, "ops": ops, "source": source, "state_evolution": state,
            "write_pattern": write_pattern, "bounded_churn_slots": slots,
        })
        for trial in range(1, final_trials + 1):
            for clients in family["clients"]:
                final_cases.append(case_template(
                    family, ops=ops, clients=int(clients), trial=trial, state_evolution=state,
                    write_pattern=write_pattern, bounded_churn_slots=slots,
                ))
    common = {
        "concurrency_probe_plan_version": PLAN_VERSION,
        "expect_trials": final_trials,
        "source_sizing_audit_sha256": sha256(audit_path),
        "source_sizing_policy_version": audit["concurrency_sizing_policy_version"],
        "target_seconds": target_s,
        **provenance,
    }
    final_plan = {
        **common,
        "kind": "steady-scaling-final",
        "family_count": len(selected),
        "families": selected,
        "case_count": len(final_cases),
        "cases": final_cases,
    }
    delete_plan = {
        **common,
        "kind": "finite-delete-diagnostic",
        "family_count": len(delete_families),
        "families": delete_families,
        "case_count": len(delete_cases),
        "cases": delete_cases,
    }
    return final_plan, delete_plan


def write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def main() -> int:
    parser = argparse.ArgumentParser(description="Plan calibrated steady-state KV concurrency performance runs")
    sub = parser.add_subparsers(dest="command", required=True)
    calibration = sub.add_parser("calibration")
    calibration.add_argument("sizing_audit", type=Path)
    calibration.add_argument("--out", type=Path, required=True)
    final = sub.add_parser("final")
    final.add_argument("sizing_audit", type=Path)
    final.add_argument("calibration_plan", type=Path)
    final.add_argument("calibration_run", type=Path)
    final.add_argument("--out", type=Path, required=True)
    final.add_argument("--delete-out", type=Path, required=True)
    final.add_argument("--trials", type=int, default=DEFAULT_FINAL_TRIALS)
    args = parser.parse_args()
    if args.command == "calibration":
        plan = build_calibration_plan(args.sizing_audit)
        write_json(args.out, plan)
        print(json.dumps({"kind": plan["kind"], "family_count": plan["family_count"], "case_count": plan["case_count"]}, sort_keys=True))
        return 0
    steady, delete = build_final_plans(
        args.sizing_audit, args.calibration_plan, args.calibration_run, final_trials=args.trials,
    )
    write_json(args.out, steady)
    write_json(args.delete_out, delete)
    print(json.dumps({
        "steady_family_count": steady["family_count"], "steady_case_count": steady["case_count"],
        "delete_family_count": delete["family_count"], "delete_case_count": delete["case_count"],
    }, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
