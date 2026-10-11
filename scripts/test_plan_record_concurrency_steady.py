from __future__ import annotations

import hashlib
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("plan-record-concurrency-steady.py")
SPEC = importlib.util.spec_from_file_location("plan_record_concurrency_steady", SCRIPT)
assert SPEC and SPEC.loader
MOD = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MOD
SPEC.loader.exec_module(MOD)

READ_MATERIALIZATION = "full-record-v1"
WRITE_MATERIALIZATION = "no-return-v1"

from concurrency_support_provenance import CONTINUOUS_ADMISSION, git_head, sha256

REPO = SCRIPT.parent.parent

def record_support() -> dict:
    return {
        "lane": "record-concurrency", "profile": "quick", "admission_policy": CONTINUOUS_ADMISSION,
        "runner_sha256": sha256(REPO / "scripts/run-record-concurrency-plan.sh"),
        "concurrency_runner_common_sha256": sha256(REPO / "scripts/concurrency-runner-common.sh"),
        "continuous_noise_common_sha256": sha256(REPO / "scripts/continuous-noise-runner-common.sh"),
        "concurrency_policy_sha256": sha256(REPO / "scripts/concurrency-matrix-policy.sh"),
        "noise_guard_sha256": sha256(REPO / "scripts/check-external-noise.py"),
        "continuous_noise_guard_sha256": sha256(REPO / "scripts/run-with-continuous-noise.py"),
        "benchmark_binary_sha256": "a" * 64, "build_profile": "external",
        "surrealdb_rocksdb_binary_sha256": "b" * 64, "rocks_build_profile": "external",
        "hostname": "test-host", "machine_id_sha256": "c" * 64, "filesystem": "zfs", "source": "testpool/scratch",
        "benchmark_source_commit": "d" * 40, "surrealdb_rocksdb_source_commit": "d" * 40, "harness_commit": git_head(REPO),
        "continuous_noise_sample_ms": 250, "continuous_noise_max_cpu_percent": 50,
        "continuous_noise_max_io_average_mib_s": 2, "continuous_noise_max_io_rate_mib_s": 8,
        "initial_min_free_gib": 10, "case_min_free_gib": "10", "case_timeout_s": 600,
        "read_materialization": READ_MATERIALIZATION, "write_materialization": WRITE_MATERIALIZATION,
    }


def case(*, workload: str, state: str, txn: int, clients: int, ops: int) -> dict[str, object]:
    return {
        "scenario": "record-concurrency-core",
        "engine": "sqlite",
        "durability": "sync",
        "workload": workload,
        "clients": clients,
        "records": 10000,
        "ops": ops,
        "payload_bytes": 512,
        "txn_size": txn,
        "trial": 1,
        "state_evolution": state,
        "read_materialization": READ_MATERIALIZATION,
        "write_materialization": WRITE_MATERIALIZATION,
    }


def plan(cases: list[dict[str, object]]) -> dict[str, object]:
    return {
        "record_concurrency_plan_version": 2,
        "read_materialization": READ_MATERIALIZATION,
        "write_materialization": WRITE_MATERIALIZATION,
        "kind": "semantic-calibration",
        "expect_trials": 1,
        "case_count": len(cases),
        "cases": cases,
    }


def write_run(root: Path, source_plan: dict[str, object], cases: list[dict[str, object]], rates: list[float]) -> tuple[Path, Path]:
    plan_path = root / "plan.json"
    plan_path.write_text(json.dumps(source_plan))
    run = root / "run"
    (run / "cases").mkdir(parents=True)
    for i, (item, rate) in enumerate(zip(cases, rates, strict=True)):
        row = dict(item)
        row["ops_requested"] = row.pop("ops")
        row["ops_completed"] = row["ops_requested"]
        row["elapsed_s"] = int(row["ops_requested"]) / rate
        row["ops_per_s"] = rate
        (run / "cases" / f"{i}.json").write_text(json.dumps(row))
    sha = hashlib.sha256(plan_path.read_bytes()).hexdigest()
    (run / "support.json").write_text(json.dumps({**record_support(), "plan_sha256": sha}))
    (run / "summary.json").write_text(
        json.dumps({"row_count": len(cases), "group_count": len(cases), "problems": []})
    )
    return plan_path, run


class Tests(unittest.TestCase):
    def test_final_plan_is_three_trial_and_transaction_aligned(self) -> None:
        cases: list[dict[str, object]] = []
        for workload, state, txn in [
            ("point-read", "growth", 100),
            ("write-burst", "bounded", 100),
        ]:
            for clients in (1, 8):
                cases.append(case(workload=workload, state=state, txn=txn, clients=clients, ops=1000))
        rates = [1000.0 if int(item["clients"]) == 1 else 100.0 for item in cases]
        with tempfile.TemporaryDirectory() as td:
            plan_path, run = write_run(Path(td), plan(cases), cases, rates)
            out = MOD.build_final(plan_path, run, 3)
        self.assertEqual(out["record_concurrency_plan_version"], 2)
        self.assertEqual(out["read_materialization"], READ_MATERIALIZATION)
        self.assertEqual(out["write_materialization"], WRITE_MATERIALIZATION)
        self.assertEqual(out["family_count"], 2)
        self.assertEqual(out["case_count"], 12)
        self.assertEqual(out["expect_trials"], 3)
        self.assertEqual(
            {item["read_materialization"] for item in out["cases"]},
            {READ_MATERIALIZATION},
        )
        self.assertEqual(
            {item["write_materialization"] for item in out["cases"]},
            {WRITE_MATERIALIZATION},
        )
        for family in out["families"]:
            self.assertLessEqual(family["projected_slowest_s"], 15.0)
            self.assertEqual(family["read_materialization"], READ_MATERIALIZATION)
            self.assertEqual(family["write_materialization"], WRITE_MATERIALIZATION)
            if family["workload"] == "write-burst":
                self.assertEqual(family["final_ops"] % 100, 0)
        for ops in {
            (item["scenario"], item["engine"], item["workload"]): item["ops"]
            for item in out["cases"]
        }.values():
            self.assertGreater(ops, 0)
        point = next(family for family in out["families"] if family["workload"] == "point-read")
        self.assertEqual(point["final_ops"], 1500)
        self.assertFalse(point["reduced_from_calibration"])

    def test_final_plan_can_reduce_overlong_calibration(self) -> None:
        cases = [
            case(workload="point-read", state="growth", txn=100, clients=clients, ops=10000)
            for clients in (1, 8)
        ]
        rates = [1000.0, 500.0]
        with tempfile.TemporaryDirectory() as td:
            plan_path, run = write_run(Path(td), plan(cases), cases, rates)
            out = MOD.build_final(plan_path, run, 3)
        family = out["families"][0]
        self.assertEqual(family["final_ops"], 3000)
        self.assertTrue(family["reduced_from_calibration"])
        self.assertAlmostEqual(family["projected_fastest_s"], 3.0)
        self.assertAlmostEqual(family["projected_slowest_s"], 6.0)

    def test_v1_calibration_plan_is_rejected(self) -> None:
        cases = [case(workload="point-read", state="growth", txn=100, clients=1, ops=1000)]
        source = plan(cases)
        source["record_concurrency_plan_version"] = 1
        with tempfile.TemporaryDirectory() as td:
            plan_path, run = write_run(Path(td), source, cases, [1000.0])
            with self.assertRaisesRegex(ValueError, "not a record semantic calibration plan"):
                MOD.build_final(plan_path, run, 3)

    def test_wrong_result_read_semantics_are_rejected(self) -> None:
        cases = [case(workload="point-read", state="growth", txn=100, clients=1, ops=1000)]
        with tempfile.TemporaryDirectory() as td:
            plan_path, run = write_run(Path(td), plan(cases), cases, [1000.0])
            result_path = next((run / "cases").glob("*.json"))
            result = json.loads(result_path.read_text())
            result["read_materialization"] = "legacy-read-v0"
            result_path.write_text(json.dumps(result))
            with self.assertRaisesRegex(ValueError, "read materialization"):
                MOD.build_final(plan_path, run, 3)

    def test_wrong_result_write_semantics_are_rejected(self) -> None:
        cases = [case(workload="write-burst", state="bounded", txn=100, clients=1, ops=1000)]
        with tempfile.TemporaryDirectory() as td:
            plan_path, run = write_run(Path(td), plan(cases), cases, [1000.0])
            result_path = next((run / "cases").glob("*.json"))
            result = json.loads(result_path.read_text())
            result["write_materialization"] = "legacy-return-v0"
            result_path.write_text(json.dumps(result))
            with self.assertRaisesRegex(ValueError, "write materialization"):
                MOD.build_final(plan_path, run, 3)


if __name__ == "__main__":
    unittest.main()
