from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("audit_baseline_sizing.py")
SPEC = importlib.util.spec_from_file_location("audit_baseline_sizing", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def row(*, engine: str, elapsed: float, rate: float, trial: int, ops: int = 50_000) -> dict[str, object]:
    return {
        "lane": "kv",
        "scenario": "baseline-core",
        "engine": engine,
        "engine_version": "1",
        "durability": "sync",
        "workload": "point-read",
        "records": 100_000,
        "ops_requested": ops,
        "clients": 1,
        "value_bytes": 256,
        "value_pattern": "pseudo-random",
        "key_bytes": 8,
        "key_shape": "sequential",
        "access_pattern": "auto",
        "miss_percent": 0,
        "write_pattern": "append",
        "txn_size": 100,
        "scan_len": 100,
        "settle_ms": 0,
        "trial": trial,
        "elapsed_s": elapsed,
        "ops_per_s": rate,
    }


class SizingAuditTests(unittest.TestCase):
    def test_classifies_accepted_undersized_and_variable(self) -> None:
        rows = [
            row(engine="accepted", elapsed=3.0, rate=100.0, trial=1),
            row(engine="accepted", elapsed=3.1, rate=102.0, trial=2),
            row(engine="accepted", elapsed=2.9, rate=98.0, trial=3),
            row(engine="short", elapsed=0.5, rate=1000.0, trial=1),
            row(engine="short", elapsed=0.5, rate=1010.0, trial=2),
            row(engine="short", elapsed=0.5, rate=990.0, trial=3),
            row(engine="variable", elapsed=5.0, rate=100.0, trial=1),
            row(engine="variable", elapsed=5.0, rate=130.0, trial=2),
            row(engine="variable", elapsed=5.0, rate=70.0, trial=3),
        ]
        report = MODULE.audit_rows(
            rows,
            expect_trials=3,
            min_seconds=2.0,
            target_seconds=3.0,
            max_cv=0.10,
            max_relative_spread=0.25,
        )
        self.assertEqual(report["counts"], {"accepted": 1, "undersized": 1, "variable": 1})
        short = next(group for group in report["groups"] if group["engine"] == "short")
        self.assertEqual(short["suggested_effective_ops"], 300_000)
        self.assertEqual(short["runner_ops_override"], 300_000)
        variable = next(group for group in report["groups"] if group["engine"] == "variable")
        self.assertIsNone(variable["suggested_effective_ops"])

    def test_tiny_txn_reports_profile_override(self) -> None:
        rows = [row(engine="tiny", elapsed=0.5, rate=1000.0, trial=trial, ops=5_000) for trial in (1, 2, 3)]
        for item in rows:
            item["workload"] = "tiny-txn"
        report = MODULE.audit_rows(
            rows,
            expect_trials=3,
            min_seconds=2.0,
            target_seconds=3.0,
            max_cv=0.10,
            max_relative_spread=0.25,
        )
        group = report["groups"][0]
        self.assertEqual(group["suggested_effective_ops"], 30_000)
        self.assertEqual(group["runner_ops_override"], 300_000)

    def test_missing_trial_is_problem(self) -> None:
        rows = [row(engine="missing", elapsed=3.0, rate=100.0, trial=trial) for trial in (1, 3)]
        report = MODULE.audit_rows(
            rows,
            expect_trials=3,
            min_seconds=2.0,
            target_seconds=3.0,
            max_cv=0.10,
            max_relative_spread=0.25,
        )
        self.assertEqual(len(report["problems"]), 1)


if __name__ == "__main__":
    unittest.main()
