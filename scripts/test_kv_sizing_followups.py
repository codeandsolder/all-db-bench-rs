from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("run-kv-sizing-followups.py")
SPEC = importlib.util.spec_from_file_location("run_kv_sizing_followups", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def group(**overrides):
    base = {
        "engine": "redb",
        "engine_version": "1",
        "durability": "sync",
        "workload": "point-read",
        "records": 100_000,
        "ops_requested": 50_000,
        "suggested_effective_ops": 300_000,
        "runner_ops_override": 300_000,
        "median_elapsed_s": 0.5,
        "status": "undersized",
    }
    base.update(overrides)
    return base


class KvSizingFollowupsTests(unittest.TestCase):
    def test_run_id_encodes_effective_count(self) -> None:
        self.assertEqual(
            MODULE.run_id(group(engine="paritydb-hash", workload="tiny-txn", suggested_effective_ops=780_000)),
            "20261006-kv-resize-paritydb-hash-sync-tiny-txn-e780000",
        )

    def test_groups_from_plan_filters_and_sorts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            plan = Path(tmp) / "plan.json"
            plan.write_text(
                json.dumps(
                    {
                        "groups": [
                            group(engine="slow", median_elapsed_s=1.0),
                            group(engine="accepted", median_elapsed_s=0.1, status="accepted"),
                            group(engine="fast", median_elapsed_s=0.05),
                        ]
                    }
                )
            )
            groups = MODULE.groups_from_plan(plan)
            self.assertEqual([item["engine"] for item in groups], ["fast", "slow"])

    def test_command_env_preserves_tiny_txn_profile_override(self) -> None:
        env = MODULE.command_env(
            group(workload="tiny-txn", suggested_effective_ops=30_000, runner_ops_override=300_000),
            Path("/tmp/pinned-kvbench"),
        )
        self.assertEqual(env["KV_OPS_OVERRIDE"], "300000")
        self.assertEqual(env["WORKLOADS_OVERRIDE"], "tiny-txn")
        self.assertEqual(env["BENCH_BIN"], "/tmp/pinned-kvbench")

    def test_complete_requires_identity_and_effective_ops(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            expected = group()
            run_dir = repo / "results" / "runs" / MODULE.run_id(expected)
            run_dir.mkdir(parents=True)
            summary = {
                "row_count": 3,
                "group_count": 1,
                "problems": [],
                "groups": [
                    {
                        "engine": "redb",
                        "durability": "sync",
                        "workload": "point-read",
                        "ops_requested": 300_000,
                    }
                ],
            }
            (run_dir / "summary.json").write_text(json.dumps(summary))
            self.assertTrue(MODULE.complete(repo, expected))
            summary["groups"][0]["ops_requested"] = 50_000
            (run_dir / "summary.json").write_text(json.dumps(summary))
            self.assertFalse(MODULE.complete(repo, expected))

    def test_calibration_uses_three_trial_median_and_bounds(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            expected = group()
            case_dir = repo / "results" / "runs" / MODULE.run_id(expected) / "cases"
            case_dir.mkdir(parents=True)
            for trial, elapsed in enumerate((2.5, 3.0, 3.5), 1):
                (case_dir / f"t{trial}.json").write_text(
                    json.dumps(
                        {
                            "engine": "redb",
                            "durability": "sync",
                            "workload": "point-read",
                            "ops_requested": 300_000,
                            "elapsed_s": elapsed,
                        }
                    )
                )
            self.assertEqual(
                MODULE.calibration_elapsed(
                    repo,
                    expected,
                    index=1,
                    count=5,
                    minimum=1.5,
                    maximum=6.0,
                ),
                3.0,
            )
            with self.assertRaises(RuntimeError):
                MODULE.calibration_elapsed(
                    repo,
                    expected,
                    index=1,
                    count=5,
                    minimum=3.1,
                    maximum=6.0,
                )
            self.assertIsNone(
                MODULE.calibration_elapsed(
                    repo,
                    expected,
                    index=6,
                    count=5,
                    minimum=1.5,
                    maximum=6.0,
                )
            )

    def test_lock_is_exclusive(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "lock"
            first = MODULE.acquire_lock(path)
            try:
                with self.assertRaises(RuntimeError):
                    MODULE.acquire_lock(path)
            finally:
                first.close()


if __name__ == "__main__":
    unittest.main()
