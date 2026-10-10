from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("refine-record-final-confirmation-plan.py")
SPEC = importlib.util.spec_from_file_location("refine_record_final_confirmation_plan", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


class RefineRecordFinalConfirmationPlanTests(unittest.TestCase):
    def test_refines_whole_undersized_family_and_preserves_other_family(self) -> None:
        groups = []
        for engine in ("fast", "slow"):
            for workload in ("point-read", "write-burst"):
                groups.append({
                    "engine": engine,
                    "durability": "sync",
                    "workload": workload,
                    "records": 100,
                    "suggested_effective_ops": 100,
                    "runner_ops_override": 100,
                    "suggested_trials": 5,
                    "quality_repair_required": True,
                    "resize_strategy": "quality-repair",
                })
        plan = {
            "quality_policy_version": 1,
            "strategy": "final-five-trial-common-work-v3",
            "common_effective_ops": {"sync/point-read": 100, "sync/write-burst": 100},
            "sizing_rule": {"slowest_ceiling_seconds": 15.0},
            "groups": groups,
        }
        thresholds = {
            "read_only_workloads": ["point-read"],
            "read_only_min_seconds": 2.0,
            "read_only_target_seconds": 3.0,
            "stateful_min_total_seconds": 0.75,
            "stateful_target_total_seconds": 1.0,
        }
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for group in groups:
                rid = M.run_id(group, "final")
                d = root / rid
                d.mkdir()
                if group["workload"] == "point-read":
                    elapsed = 0.5 if group["engine"] == "fast" else (100.0 / 30.0)
                    rate = 200.0 if group["engine"] == "fast" else 30.0
                else:
                    elapsed = 1.0
                    rate = 100.0
                rows = [{
                    "engine": group["engine"],
                    "durability": group["durability"],
                    "workload": group["workload"],
                    "records": group["records"],
                    "ops_requested": 100,
                    "trial": trial,
                    "elapsed_s": elapsed,
                    "ops_per_s": rate,
                } for trial in range(1, 6)]
                (d / "results.ndjson").write_text(
                    "".join(json.dumps(row) + "\n" for row in rows)
                )
                (d / "summary.json").write_text(
                    json.dumps({"row_count": 5, "group_count": 1, "problems": []})
                )

            refined = M.refine_plan(
                plan,
                base_plan_sha256="a" * 64,
                run_root=root,
                thresholds=thresholds,
                run_prefix="final",
            )

        self.assertEqual(refined["strategy"], "final-five-trial-common-work-v5")
        self.assertEqual(set(refined["refined_families"]), {"sync/point-read"})
        self.assertEqual(
            {g["suggested_effective_ops"] for g in refined["groups"] if g["workload"] == "point-read"},
            {400},
        )
        self.assertTrue(refined["refined_families"]["sync/point-read"]["slowest_ceiling_feasible"])
        self.assertLessEqual(refined["refined_families"]["sync/point-read"]["estimated_slowest_seconds"], 15.0)
        self.assertEqual(
            {g["suggested_effective_ops"] for g in refined["groups"] if g["workload"] == "write-burst"},
            {100},
        )
        self.assertEqual(len(refined["source_confirmation_results_sha256"]), 4)


if __name__ == "__main__":
    unittest.main()
