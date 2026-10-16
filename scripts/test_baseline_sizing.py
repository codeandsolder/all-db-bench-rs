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


def row(
    *,
    engine: str,
    elapsed: float,
    rate: float,
    trial: int,
    ops: int = 50_000,
    lane: str = "kv",
    workload: str = "point-read",
) -> dict[str, object]:
    return {
        "lane": lane,
        "scenario": "baseline-core",
        "engine": engine,
        "engine_version": "1",
        "durability": "sync",
        "workload": workload,
        "records": 100_000 if lane == "kv" else 10_000,
        "ops_requested": ops,
        "clients": 1,
        "value_bytes": 256 if lane == "kv" else 512,
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


def audit(rows: list[dict[str, object]]) -> dict[str, object]:
    return MODULE.audit_rows(
        rows,
        expect_trials=3,
        read_only_min_seconds=2.0,
        read_only_target_seconds=3.0,
        stateful_min_total_seconds=0.75,
        stateful_target_total_seconds=1.0,
        stateful_max_trials=51,
        max_cv=0.10,
        max_relative_spread=0.25,
    )


class SizingAuditTests(unittest.TestCase):
    def test_read_only_classifies_and_resizes_ops(self) -> None:
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
        report = audit(rows)
        self.assertEqual(report["counts"], {"accepted": 1, "undersized": 1, "variable": 1})
        short = next(group for group in report["groups"] if group["engine"] == "short")
        self.assertEqual(short["resize_strategy"], "more-ops")
        self.assertEqual(short["suggested_effective_ops"], 300_000)
        self.assertEqual(short["runner_ops_override"], 300_000)
        self.assertEqual(short["suggested_trials"], 3)

    def test_stateful_short_group_preserves_ops_and_adds_trials(self) -> None:
        rows = [
            row(engine="tiny", elapsed=0.1, rate=50_000.0, trial=trial, ops=5_000, workload="tiny-txn")
            for trial in (1, 2, 3)
        ]
        group = audit(rows)["groups"][0]
        self.assertEqual(group["status"], "undersized")
        self.assertEqual(group["resize_strategy"], "more-trials")
        self.assertEqual(group["suggested_effective_ops"], 5_000)
        self.assertEqual(group["runner_ops_override"], 50_000)
        self.assertEqual(group["suggested_trials"], 11)

    def test_record_tiny_txn_uses_record_profile_divisor(self) -> None:
        rows = [
            row(
                engine="record",
                elapsed=0.1,
                rate=50_000.0,
                trial=trial,
                ops=5_000,
                lane="record",
                workload="tiny-txn",
            )
            for trial in (1, 2, 3)
        ]
        group = audit(rows)["groups"][0]
        self.assertEqual(group["runner_ops_override"], 10_000)
        self.assertEqual(group["suggested_effective_ops"], 5_000)

    def test_stateful_group_with_enough_aggregate_time_is_not_resized(self) -> None:
        rows = [
            row(engine="burst", elapsed=0.3, rate=100_000.0, trial=trial, workload="write-burst")
            for trial in (1, 2, 3)
        ]
        group = audit(rows)["groups"][0]
        self.assertEqual(group["status"], "accepted")
        self.assertIsNone(group["resize_strategy"])

    def test_stateful_trial_expansion_is_capped_and_odd(self) -> None:
        rows = [
            row(engine="fast", elapsed=0.02, rate=500_000.0, trial=trial, workload="write-burst")
            for trial in (1, 2, 3)
        ]
        report = audit(rows)
        group = report["groups"][0]
        self.assertEqual(group["suggested_trials"], 51)
        self.assertEqual(group["minimum_required_trials"], 39)
        self.assertFalse(group["sampling_cap_insufficient"])
        self.assertEqual(report["problems"], [])

    def test_stateful_cap_fails_closed_if_minimum_cannot_be_met(self) -> None:
        rows = [
            row(engine="too-fast", elapsed=0.01, rate=500_000.0, trial=trial, workload="write-burst")
            for trial in (1, 2, 3)
        ]
        report = audit(rows)
        group = report["groups"][0]
        self.assertEqual(group["suggested_trials"], 51)
        self.assertEqual(group["minimum_required_trials"], 75)
        self.assertTrue(group["sampling_cap_insufficient"])
        self.assertEqual(len(report["problems"]), 1)
        self.assertIn("requires at least 75 trials", report["problems"][0])

    def test_policy_v3_and_record_read_semantics_are_group_identity(self) -> None:
        self.assertEqual(MODULE.SIZING_POLICY_VERSION, 3)
        corrected = [
            dict(
                row(engine="sqlite", elapsed=3.0, rate=100.0, trial=trial, lane="record"),
                read_materialization="full-record-v1",
                write_materialization="no-return-v1",
            )
            for trial in (1, 2, 3)
        ]
        legacy = [
            dict(
                row(engine="sqlite", elapsed=3.0, rate=100.0, trial=trial, lane="record"),
                read_materialization="legacy-read-v0",
                write_materialization="no-return-v1",
            )
            for trial in (1, 2, 3)
        ]
        report = audit(corrected + legacy)
        self.assertEqual(report["sizing_policy_version"], 3)
        self.assertEqual(report["group_count"], 2)
        self.assertEqual(
            {group["read_materialization"] for group in report["groups"]},
            {"full-record-v1", "legacy-read-v0"},
        )

    def test_record_write_materialization_is_group_identity(self) -> None:
        corrected = [
            dict(row(engine="sqlite", elapsed=3.0, rate=100.0, trial=trial, lane="record"),
                 read_materialization="full-record-v1", write_materialization="no-return-v1")
            for trial in (1, 2, 3)
        ]
        legacy = [
            dict(row(engine="sqlite", elapsed=3.0, rate=100.0, trial=trial, lane="record"),
                 read_materialization="full-record-v1", write_materialization="legacy-return-v0")
            for trial in (1, 2, 3)
        ]
        report = audit(corrected + legacy)
        self.assertEqual(report["group_count"], 2)
        self.assertEqual(
            {group["write_materialization"] for group in report["groups"]},
            {"no-return-v1", "legacy-return-v0"},
        )

    def test_missing_trial_is_problem(self) -> None:
        rows = [row(engine="missing", elapsed=3.0, rate=100.0, trial=trial) for trial in (1, 3)]
        report = audit(rows)
        self.assertEqual(len(report["problems"]), 1)


if __name__ == "__main__":
    unittest.main()
