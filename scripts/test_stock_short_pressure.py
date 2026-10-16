from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("audit-stock-short-pressure.py")
SPEC = importlib.util.spec_from_file_location("audit_stock_short_pressure", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def group(engine: str, status: str, workload: str = "write-burst"):
    return {
        "engine": engine,
        "engine_version": "1",
        "durability": "relaxed",
        "workload": workload,
        "records": 100_000,
        "ops_requested": 50_000 if workload != "tiny-txn" else 5_000,
        "trials": [1, 2, 3],
        "median_elapsed_s": 0.3,
        "status": status,
        "suggested_effective_ops": None,
        "runner_ops_override": None,
        "suggested_trials": None,
        "resize_strategy": None,
    }


class StockShortPressureTests(unittest.TestCase):
    def test_retained_group_gets_quality_repair_and_undersized_is_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            (run / "results.ndjson").write_text("{}\n")
            audit = {
                "sizing_policy_version": 2,
                "thresholds": {"expect_trials": 3},
                "groups": [group("keep", "variable"), group("replace", "undersized")],
            }
            report = {
                "thresholds": {"max_elapsed_s": 0.5},
                "available_cpus": 8,
                "rejected": 2,
                "cases": [
                    {"engine": "keep", "durability": "relaxed", "workload": "write-burst", "trial": 2},
                    {"engine": "replace", "durability": "relaxed", "workload": "write-burst", "trial": 1},
                ],
            }
            plan = MODULE.build_plan(run, audit, report, lane="kv")
        self.assertEqual(plan["repair_group_count"], 1)
        self.assertEqual(plan["ignored_undersized_group_count"], 1)
        repair = plan["groups"][0]
        self.assertEqual(repair["engine"], "keep")
        self.assertEqual(repair["resize_strategy"], "quality-repair")
        self.assertEqual(repair["suggested_effective_ops"], 50_000)
        self.assertEqual(repair["runner_ops_override"], 50_000)
        self.assertEqual(repair["suggested_trials"], 3)
        self.assertEqual(repair["pressure_trials"], [2])

    def test_record_pressure_identity_includes_materialization_semantics(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            (run / "results.ndjson").write_text("{}\n")
            corrected = group("sqlite", "accepted", workload="point-read")
            corrected.update(read_materialization="full-record-v1", write_materialization="no-return-v1")
            legacy = dict(corrected, read_materialization="legacy-read-v0", write_materialization="legacy-return-v0")
            audit = {
                "sizing_policy_version": 3,
                "thresholds": {"expect_trials": 3},
                "groups": [corrected, legacy],
            }
            report = {
                "thresholds": {"max_elapsed_s": 0.5},
                "available_cpus": 8,
                "rejected": 1,
                "cases": [{
                    "engine": "sqlite", "durability": "relaxed", "workload": "point-read",
                    "read_materialization": "full-record-v1",
                    "write_materialization": "no-return-v1",
                    "trial": 2,
                }],
            }
            plan = MODULE.build_plan(run, audit, report, lane="record")
        self.assertEqual(plan["repair_group_count"], 1)
        repair = plan["groups"][0]
        self.assertEqual(repair["read_materialization"], "full-record-v1")
        self.assertEqual(repair["write_materialization"], "no-return-v1")

    def test_tiny_txn_runner_override_is_lane_specific(self) -> None:
        self.assertEqual(MODULE.runner_ops_override("kv", "tiny-txn", 5_000), 50_000)
        self.assertEqual(MODULE.runner_ops_override("record", "tiny-txn", 5_000), 10_000)


if __name__ == "__main__":
    unittest.main()
