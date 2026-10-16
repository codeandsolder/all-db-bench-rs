from __future__ import annotations

import importlib.util
import json
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def load(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


GEN = load("record_plan_gen", "make-record-concurrency-calibration-plan.py")
VAL = load("record_plan_val", "validate-record-concurrency-plan.py")


class RunnerRegressionTests(unittest.TestCase):
    def test_pinned_hashes_are_not_rehashed_by_inner_runner(self) -> None:
        runner = (ROOT / "scripts" / "run-record-concurrency-plan.sh").read_text()
        self.assertNotIn('actual=$(sha256sum "$BIN"', runner)
        self.assertNotIn('actual=$(sha256sum "$ROCKS_BIN"', runner)
        self.assertIn('BIN_SHA="$BENCH_BIN_SHA256"', runner)
        self.assertIn('ROCKS_BIN_SHA="$ROCKS_BENCH_BIN_SHA256"', runner)
        self.assertIn("read_materialization", runner)
        self.assertIn("write_materialization", runner)


class RecordConcurrencyPlanTests(unittest.TestCase):
    def test_calibration_cardinality_and_semantics(self) -> None:
        plan = GEN.build_plan(ROOT)
        self.assertEqual(plan["record_concurrency_plan_version"], 2)
        self.assertEqual(plan["read_materialization"], "full-record-v1")
        self.assertEqual(plan["write_materialization"], "no-return-v1")
        self.assertEqual(plan["case_count"], 164)
        self.assertEqual(plan["expect_trials"], 1)
        self.assertEqual(
            {case["read_materialization"] for case in plan["cases"]},
            {"full-record-v1"},
        )
        self.assertEqual(
            {case["write_materialization"] for case in plan["cases"]},
            {"no-return-v1"},
        )
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
        self.assertEqual(meta["read_materialization"], "full-record-v1")
        self.assertEqual(meta["write_materialization"], "no-return-v1")
        self.assertTrue(all(row[-2] == "full-record-v1" for row in rows))
        self.assertTrue(all(row[-1] == "no-return-v1" for row in rows))

    def test_policy_budgets_match_expected_quick_caps(self) -> None:
        plan = GEN.build_plan(ROOT)

        def find(scenario: str, workload: str, txn: int = 100):
            return next(
                case
                for case in plan["cases"]
                if case["scenario"] == scenario
                and case["workload"] == workload
                and case["txn_size"] == txn
            )

        self.assertEqual(find("record-concurrency-core", "point-read")["ops"], 6000)
        self.assertEqual(find("record-concurrency-core", "tiny-txn")["ops"], 2000)
        self.assertEqual(find("record-concurrency-txn-1", "write-burst", 1)["ops"], 1400)
        self.assertEqual(find("record-concurrency-txn-1000", "write-burst", 1000)["ops"], 8000)

    def test_final_multitrial_family_validates(self) -> None:
        cases = []
        for clients in (1, 2, 4, 8):
            for trial in (1, 2, 3):
                cases.append(
                    {
                        "scenario": "record-concurrency-core",
                        "engine": "sqlite",
                        "durability": "sync",
                        "workload": "write-burst",
                        "read_materialization": "full-record-v1",
                        "write_materialization": "no-return-v1",
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
            "record_concurrency_plan_version": 2,
            "read_materialization": "full-record-v1",
            "write_materialization": "no-return-v1",
            "kind": "steady-final",
            "expect_trials": 3,
            "case_count": len(cases),
            "cases": cases,
        }
        rows, meta = VAL.materialize(plan)
        self.assertEqual(len(rows), 12)
        self.assertEqual(meta["expect_trials"], 3)

    def test_v1_plan_is_rejected(self) -> None:
        plan = GEN.build_plan(ROOT)
        plan["record_concurrency_plan_version"] = 1
        with self.assertRaisesRegex(ValueError, "unsupported record concurrency plan version"):
            VAL.materialize(plan)

    def test_wrong_top_level_read_semantics_are_rejected(self) -> None:
        plan = GEN.build_plan(ROOT)
        plan["read_materialization"] = "legacy-read-v0"
        with self.assertRaisesRegex(ValueError, "invalid plan read_materialization"):
            VAL.materialize(plan)

    def test_wrong_case_read_semantics_are_rejected(self) -> None:
        plan = GEN.build_plan(ROOT)
        plan["cases"][0]["read_materialization"] = "legacy-read-v0"
        with self.assertRaisesRegex(ValueError, "invalid read_materialization"):
            VAL.materialize(plan)

    def test_missing_case_read_semantics_are_rejected(self) -> None:
        plan = GEN.build_plan(ROOT)
        del plan["cases"][0]["read_materialization"]
        with self.assertRaisesRegex(ValueError, "missing"):
            VAL.materialize(plan)

    def test_wrong_top_level_write_semantics_are_rejected(self) -> None:
        plan = GEN.build_plan(ROOT)
        plan["write_materialization"] = "legacy-return-v0"
        with self.assertRaisesRegex(ValueError, "invalid plan write_materialization"):
            VAL.materialize(plan)

    def test_wrong_case_write_semantics_are_rejected(self) -> None:
        plan = GEN.build_plan(ROOT)
        plan["cases"][0]["write_materialization"] = "legacy-return-v0"
        with self.assertRaisesRegex(ValueError, "invalid write_materialization"):
            VAL.materialize(plan)

    def test_missing_case_write_semantics_are_rejected(self) -> None:
        plan = GEN.build_plan(ROOT)
        del plan["cases"][0]["write_materialization"]
        with self.assertRaisesRegex(ValueError, "missing"):
            VAL.materialize(plan)

    def test_bounded_nonwrite_rejected(self) -> None:
        plan = GEN.build_plan(ROOT)
        bad = json.loads(json.dumps(plan))
        bad["cases"][0]["state_evolution"] = "bounded"
        with self.assertRaisesRegex(ValueError, "bounded state unsupported"):
            VAL.materialize(bad)


if __name__ == "__main__":
    unittest.main()
