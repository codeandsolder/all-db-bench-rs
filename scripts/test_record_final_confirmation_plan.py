from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("make-record-final-confirmation-plan.py")
SPEC = importlib.util.spec_from_file_location("make_record_final_confirmation_plan", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


class RecordFinalConfirmationPlanTests(unittest.TestCase):
    def rows(self):
        rates = {"sqlite": 1000.0, "surrealdb": 500.0, "surrealdb-rocksdb": 250.0, "turso": 2000.0}
        rows = []
        for engine in M.PRODUCTS:
            for durability in M.DURABILITIES:
                for workload in M.WORKLOADS:
                    rows.append({
                        "engine": engine,
                        "durability": durability,
                        "workload": workload,
                        "records": 10000,
                        "ops_per_s": rates[engine],
                        "read_materialization": "full-record-v1",
                        "write_materialization": "no-return-v1",
                    })
        return rows

    def test_common_work_and_duration_ceiling(self):
        plan = M.build_plan(self.rows(), source_sha256="a" * 64)
        self.assertEqual(plan["group_count"], 40)
        self.assertEqual(plan["trials_per_group"], 5)
        for durability in M.DURABILITIES:
            for workload in M.WORKLOADS:
                same = [g["suggested_effective_ops"] for g in plan["groups"] if g["durability"] == durability and g["workload"] == workload]
                self.assertEqual(len(set(same)), 1)
                self.assertLessEqual(plan["estimated_duration_bounds"][f"{durability}/{workload}"]["estimated_slowest_s"], 15.0)

    def test_tiny_txn_runner_override_accounts_for_quick_runner_halving(self):
        plan = M.build_plan(self.rows(), source_sha256="b" * 64)
        for group in plan["groups"]:
            if group["workload"] == "tiny-txn":
                self.assertEqual(group["runner_ops_override"], 2 * group["suggested_effective_ops"])

    def test_wrong_materialization_fails_closed(self):
        rows = self.rows()
        rows[0]["read_materialization"] = "legacy-read-v0"
        with self.assertRaisesRegex(ValueError, "materialization"):
            M.build_plan(rows, source_sha256="c" * 64)


if __name__ == "__main__":
    unittest.main()
