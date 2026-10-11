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

from concurrency_support_provenance import CONTINUOUS_ADMISSION, git_head, sha256, support_identity

REPO = SCRIPT.parent.parent

def kv_support() -> dict:
    return {
        "lane": "kv-concurrency", "profile": "quick", "admission_policy": CONTINUOUS_ADMISSION,
        "runner_sha256": sha256(REPO / "scripts/run-kv-concurrency-plan.sh"),
        "concurrency_runner_common_sha256": sha256(REPO / "scripts/concurrency-runner-common.sh"),
        "continuous_noise_common_sha256": sha256(REPO / "scripts/continuous-noise-runner-common.sh"),
        "concurrency_policy_sha256": sha256(REPO / "scripts/concurrency-matrix-policy.sh"),
        "noise_guard_sha256": sha256(REPO / "scripts/check-external-noise.py"),
        "continuous_noise_guard_sha256": sha256(REPO / "scripts/run-with-continuous-noise.py"),
        "benchmark_binary_sha256": "a" * 64, "build_profile": "external",
        "hostname": "test-host", "machine_id_sha256": "b" * 64, "filesystem": "zfs", "source": "testpool/scratch",
        "benchmark_source_commit": "c" * 40, "harness_commit": git_head(REPO),
        "continuous_noise_sample_ms": 250, "continuous_noise_max_cpu_percent": 50,
        "continuous_noise_max_io_average_mib_s": 2, "continuous_noise_max_io_rate_mib_s": 8,
        "initial_min_free_gib": 10, "case_min_free_gib": "10", "case_timeout_s": 600,
        "persy_lock_timeout_ms": 250, "prepared_db_protocol": "case-private-clean-close-v1",
    }

def kv_identity() -> dict:
    return support_identity(kv_support(), expected_lane="kv-concurrency", repo=REPO, runner_name="run-kv-concurrency-plan.sh")


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
        "measurement_identity": kv_identity(),
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
            (run/"support.json").write_text(json.dumps({**kv_support(), "plan_sha256": M.sha256(plan_path)}))
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

    def test_calibration_support_must_match_sizing_measurement_identity(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root=Path(td); audit=root/"audit.json"; write_audit(audit,[family("balanced",status="sized",rate=1000)])
            plan=M.build_calibration_plan(audit); pp=root/"plan.json"; M.write_json(pp,plan)
            run=root/"run"; (run/"cases").mkdir(parents=True)
            support=kv_support(); support["hostname"]="other-host"; support["plan_sha256"]=M.sha256(pp)
            (run/"support.json").write_text(json.dumps(support))
            with self.assertRaisesRegex(ValueError,"measurement identity differs"):
                M.load_calibration_results(pp,run)

    def test_calibration_support_must_bind_plan(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root=Path(td); audit=root/"audit.json"; write_audit(audit,[family("balanced",status="sized",rate=1000)])
            plan=M.build_calibration_plan(audit); pp=root/"plan.json"; M.write_json(pp,plan)
            run=root/"run"; (run/"cases").mkdir(parents=True); (run/"support.json").write_text(json.dumps({**kv_support(), "plan_sha256":"bad"}))
            with self.assertRaisesRegex(ValueError,"plan SHA mismatch"):
                M.load_calibration_results(pp,run)


if __name__ == "__main__":
    unittest.main()
