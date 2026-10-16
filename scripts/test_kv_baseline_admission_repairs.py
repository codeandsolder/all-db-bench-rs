from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("plan-kv-baseline-admission-repairs.py")
SPEC = importlib.util.spec_from_file_location("plan_kv_baseline_admission_repairs", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


def group(engine: str, workload: str, *, trials: int = 7, ops: int = 50_000) -> dict:
    return {
        "engine": engine,
        "engine_version": "1",
        "durability": "relaxed",
        "workload": workload,
        "records": 100_000,
        "ops_requested": ops,
        "status": "undersized",
        "resize_strategy": "more-trials",
        "runner_ops_override": 50_000,
        "suggested_effective_ops": ops,
        "suggested_trials": trials,
    }


class KvBaselineAdmissionRepairPlannerTests(unittest.TestCase):
    def test_selects_only_sources_with_legacy_rejections_and_preserves_geometry(self) -> None:
        bad = group("rocksdb", "write-burst", trials=33)
        clean = group("redb", "tiny-txn", trials=15, ops=5_000)
        audit = {"sizing_policy_version": 2, "groups": [bad, clean]}
        manifest = {
            "complete": True,
            "selected_followup_rejected_pressure_attempts": 6,
            "sources": [
                {
                    "source": "resize",
                    "run_id": M.legacy_run_id(bad),
                    "engine": "rocksdb",
                    "durability": "relaxed",
                    "workload": "write-burst",
                    "ops_requested": 50_000,
                    "trials": 33,
                    "final_status": "accepted",
                    "rejected_pressure_attempts": 6,
                },
                {
                    "source": "resize",
                    "run_id": M.legacy_run_id(clean),
                    "engine": "redb",
                    "durability": "relaxed",
                    "workload": "tiny-txn",
                    "ops_requested": 5_000,
                    "trials": 15,
                    "final_status": "accepted",
                    "rejected_pressure_attempts": 0,
                },
            ],
        }
        plan = M.build_plan(audit, manifest, manifest_sha256="abc")
        self.assertEqual(plan["suspect_source_count"], 1)
        self.assertEqual(plan["legacy_rejected_pressure_attempts"], 6)
        self.assertEqual(plan["source_manifest_sha256"], "abc")
        repair = plan["groups"][0]
        self.assertEqual(repair["suggested_trials"], 33)
        self.assertEqual(repair["runner_ops_override"], 50_000)
        self.assertEqual(repair["resize_strategy"], "quality-repair")
        self.assertTrue(repair["quality_repair_required"])

    def test_fails_if_suspect_source_cannot_be_reconstructed(self) -> None:
        manifest = {
            "complete": True,
            "selected_followup_rejected_pressure_attempts": 1,
            "sources": [
                {
                    "run_id": "missing",
                    "engine": "x",
                    "durability": "relaxed",
                    "workload": "tiny-txn",
                    "ops_requested": 1,
                    "trials": 3,
                    "rejected_pressure_attempts": 1,
                }
            ],
        }
        with self.assertRaisesRegex(ValueError, "not found"):
            M.build_plan({"sizing_policy_version": 2, "groups": []}, manifest, manifest_sha256="x")

if __name__ == "__main__":
    unittest.main()
