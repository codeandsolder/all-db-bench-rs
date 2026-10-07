from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("plan-kv-concurrency-steady.py")
SPEC = importlib.util.spec_from_file_location("plan_kv_concurrency_steady", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(M)


def family(workload: str, *, status: str, rate: float, ops: int = 50_000, clients=(1, 8)) -> dict:
    fixed = workload in M.FIXED_WORKLOADS
    return {
        "scenario": "concurrency-range" if workload == "range-scan" else "concurrency-primary",
        "engine": "x", "durability": "sync", "workload": workload,
        "clients": list(clients), "missing_clients": [], "current_records": 100_000,
        "current_ops": ops, "observed_client_groups": len(clients), "planned_client_groups": len(clients),
        "semantics": "fixed-keyspace" if fixed else "state-changing", "status": status,
        "recommendation": "more-ops" if fixed and status == "undersized" else "keep-stock",
        "suggested_ops": max(ops, M.rounded_ops(rate * 3.0)) if fixed else ops,
        "suggested_trials": 3, "fastest_observed_median_elapsed_s": ops / rate,
        "max_observed_median_ops_per_s": rate,
    }


def write_audit(path: Path, families: list[dict], *, missing: int = 0) -> None:
    path.write_text(json.dumps({
        "concurrency_sizing_policy_version": 1, "missing_probe_case_count": missing,
        "planned_family_count": len(families), "thresholds": {"target_seconds": 3.0}, "families": families,
    }))


class SteadyPlannerTests(unittest.TestCase):
    def test_incomplete_audit_refuses_plan(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            p = Path(td)/"audit.json"; write_audit(p, [family("point-read", status="undersized", rate=1000)], missing=1)
            with self.assertRaisesRegex(ValueError, "missing client groups"):
                M.build_calibration_plan(p)

    def test_calibration_only_probes_fixed_undersized_and_bounded(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            p = Path(td)/"audit.json"
            fs = [
                family("point-read", status="undersized", rate=100_000, ops=200_000),
                family("range-scan", status="sized", rate=300, ops=1_000),
                family("balanced", status="sized", rate=200_000),
                family("delete-burst", status="sized", rate=20_000),
            ]
            write_audit(p, fs)
            plan = M.build_calibration_plan(p)
            self.assertEqual(plan["family_count"], 2)
            self.assertEqual(plan["case_count"], 4)
            by_workload = {c["workload"]: c for c in plan["cases"]}
            self.assertEqual(by_workload["point-read"]["state_evolution"], "growth")
            self.assertEqual(by_workload["point-read"]["ops"], 500_000)
            self.assertEqual(by_workload["balanced"]["state_evolution"], "bounded")
            self.assertEqual(by_workload["balanced"]["write_pattern"], "update-uniform")
            self.assertEqual(by_workload["balanced"]["bounded_churn_slots"], 8192)

    def test_final_plan_uses_calibration_and_splits_delete(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td); audit = root/"audit.json"
            fs = [
                family("point-read", status="undersized", rate=100_000, ops=200_000),
                family("range-scan", status="sized", rate=300, ops=1_000),
                family("balanced", status="sized", rate=200_000),
                family("delete-burst", status="sized", rate=20_000),
            ]
            write_audit(audit, fs)
            calibration = M.build_calibration_plan(audit)
            plan_path = root/"plan.json"; M.write_json(plan_path, calibration)
            run = root/"run"; (run/"cases").mkdir(parents=True)
            (run/"support.json").write_text(json.dumps({"plan_sha256": M.sha256(plan_path)}))
            for i, case in enumerate(calibration["cases"]):
                row = dict(case)
                row["ops_requested"] = row.pop("ops")
                row["ops_completed"] = row["ops_requested"]
                # Calibrated max rate 150k for point reads -> 500k rounded target; 250k balanced -> 1m.
                rate = 150_000.0 if row["workload"] == "point-read" else 250_000.0
                row["ops_per_s"] = rate
                row["elapsed_s"] = row["ops_requested"] / rate
                (run/"cases"/f"c{i}.json").write_text(json.dumps(row))
            steady, delete = M.build_final_plans(audit, plan_path, run, final_trials=3)
            self.assertEqual(steady["family_count"], 3)
            self.assertEqual(delete["family_count"], 1)
            self.assertEqual(delete["case_count"], 6)
            point = next(c for c in steady["cases"] if c["workload"] == "point-read")
            balanced = next(c for c in steady["cases"] if c["workload"] == "balanced")
            scan = next(c for c in steady["cases"] if c["workload"] == "range-scan")
            self.assertEqual(point["ops"], 500_000)
            self.assertEqual(balanced["ops"], 1_000_000)
            self.assertEqual(balanced["state_evolution"], "bounded")
            self.assertEqual(scan["ops"], 1_000)
            self.assertEqual(steady["expect_trials"], 3)
            self.assertEqual({c["trial"] for c in steady["cases"]}, {1,2,3})

    def test_calibration_support_must_bind_plan(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root=Path(td); audit=root/"audit.json"; write_audit(audit,[family("balanced",status="sized",rate=1000)])
            plan=M.build_calibration_plan(audit); pp=root/"plan.json"; M.write_json(pp,plan)
            run=root/"run"; (run/"cases").mkdir(parents=True); (run/"support.json").write_text(json.dumps({"plan_sha256":"bad"}))
            with self.assertRaisesRegex(ValueError,"plan SHA mismatch"):
                M.load_calibration_results(pp,run)


if __name__ == "__main__":
    unittest.main()
