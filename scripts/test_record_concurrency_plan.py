from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def load(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


GEN = load("record_plan_gen", "make-record-concurrency-calibration-plan.py")
VAL = load("record_plan_val", "validate-record-concurrency-plan.py")


class RecordConcurrencyPlanTests(unittest.TestCase):
    def test_calibration_cardinality_and_semantics(self):
        plan = GEN.build_plan(ROOT)
        self.assertEqual(plan["case_count"], 164)
        self.assertEqual(plan["expect_trials"], 1)
        bounded = [case for case in plan["cases"] if case["state_evolution"] == "bounded"]
        self.assertTrue(bounded)
        self.assertTrue(all(case["workload"] in {"tiny-txn", "write-burst"} for case in bounded))
        self.assertTrue(
            all(
                case["state_evolution"] == "bounded"
                for case in plan["cases"]
                if case["workload"] in {"tiny-txn", "write-burst"}
            )
        )
        rows, meta = VAL.materialize(plan)
        self.assertEqual(len(rows), 164)
        self.assertEqual(meta["expect_trials"], 1)

    def test_policy_budgets_match_expected_quick_caps(self):
        plan = GEN.build_plan(ROOT)
        def find(scenario, workload, txn=100):
            return next(
                case for case in plan["cases"]
                if case["scenario"] == scenario
                and case["workload"] == workload
                and case["txn_size"] == txn
            )
        self.assertEqual(find("record-concurrency-core", "point-read")["ops"], 6000)
        self.assertEqual(find("record-concurrency-core", "tiny-txn")["ops"], 2000)
        self.assertEqual(find("record-concurrency-txn-1", "write-burst", 1)["ops"], 1400)
        self.assertEqual(find("record-concurrency-txn-1000", "write-burst", 1000)["ops"], 8000)

    def test_final_multitrial_family_validates(self):
        cases = []
        for clients in (1, 2, 4, 8):
            for trial in (1, 2, 3):
                cases.append(
                    {
                        "scenario": "record-concurrency-core",
                        "engine": "sqlite",
                        "durability": "sync",
                        "workload": "write-burst",
                        "clients": clients,
                        "records": 10000,
                        "ops": 12000,
                        "payload_bytes": 512,
                        "txn_size": 100,
                        "trial": trial,
                        "state_evolution": "bounded",
                    }
                )
        plan = {
            "record_concurrency_plan_version": 1,
            "kind": "steady-final",
            "expect_trials": 3,
            "case_count": len(cases),
            "cases": cases,
        }
        rows, meta = VAL.materialize(plan)
        self.assertEqual(len(rows), 12)
        self.assertEqual(meta["expect_trials"], 3)

    def test_bounded_nonwrite_rejected(self):
        plan = GEN.build_plan(ROOT)
        bad = json.loads(json.dumps(plan))
        bad["cases"][0]["state_evolution"] = "bounded"
        with self.assertRaisesRegex(ValueError, "bounded state unsupported"):
            VAL.materialize(bad)


if __name__ == "__main__":
    unittest.main()
