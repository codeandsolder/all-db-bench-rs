from __future__ import annotations

import importlib.util
import json
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
        "suggested_trials": 3,
        "resize_strategy": "more-ops",
        "median_elapsed_s": 0.5,
        "status": "undersized",
    }
    base.update(overrides)
    return base


class KvSizingFollowupsTests(unittest.TestCase):
    def test_run_id_encodes_effective_count_and_trials(self) -> None:
        self.assertEqual(
            MODULE.run_id(group(engine="paritydb-hash", suggested_effective_ops=780_000)),
            "20261006-kv-resize-v4-paritydb-hash-sync-point-read-e780000-t3",
        )

    def test_groups_from_plan_requires_v2_and_calibrates_more_ops_first(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            plan = Path(tmp) / "plan.json"
            plan.write_text(
                json.dumps(
                    {
                        "sizing_policy_version": 2,
                        "groups": [
                            group(engine="state", median_elapsed_s=0.01, resize_strategy="more-trials", suggested_trials=51),
                            group(engine="read", median_elapsed_s=1.0),
                            group(engine="accepted", median_elapsed_s=0.1, status="accepted"),
                        ],
                    }
                )
            )
            quality = Path(tmp) / "quality.json"
            quality.write_text(
                json.dumps(
                    {
                        "quality_policy_version": 1,
                        "groups": [
                            group(
                                engine="repair",
                                status="variable",
                                resize_strategy="quality-repair",
                                suggested_effective_ops=50_000,
                                runner_ops_override=50_000,
                                suggested_trials=3,
                                median_elapsed_s=0.3,
                            )
                        ],
                    }
                )
            )
            groups = MODULE.groups_from_plan(plan, quality)
            self.assertEqual([item["engine"] for item in groups], ["read", "state", "repair"])
            plan.write_text(json.dumps({"sizing_policy_version": 1, "groups": []}))
            with self.assertRaises(ValueError):
                MODULE.groups_from_plan(plan)

    def test_command_env_preserves_stateful_ops_and_expands_trials(self) -> None:
        env = MODULE.command_env(
            group(
                workload="tiny-txn",
                suggested_effective_ops=5_000,
                runner_ops_override=50_000,
                suggested_trials=11,
                resize_strategy="more-trials",
            ),
            Path("/tmp/pinned-kvbench"),
        )
        self.assertEqual(env["KV_OPS_OVERRIDE"], "50000")
        self.assertEqual(env["KV_TRIALS_OVERRIDE"], "11")
        self.assertEqual(env["WORKLOADS_OVERRIDE"], "tiny-txn")
        self.assertEqual(env["BENCH_BIN"], "/tmp/pinned-kvbench")

    def test_complete_requires_identity_effective_ops_and_trial_count(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            expected = group(suggested_trials=5)
            run_dir = repo / "results" / "runs" / MODULE.run_id(expected)
            run_dir.mkdir(parents=True)
            summary = {
                "row_count": 5,
                "group_count": 1,
                "problems": [],
                "groups": [
                    {
                        "engine": "redb",
                        "durability": "sync",
                        "workload": "point-read",
                        "ops_requested": 300_000,
                        "trials": [1, 2, 3, 4, 5],
                    }
                ],
            }
            (run_dir / "summary.json").write_text(json.dumps(summary))
            self.assertTrue(MODULE.complete(repo, expected))
            summary["groups"][0]["trials"] = [1, 2, 3]
            (run_dir / "summary.json").write_text(json.dumps(summary))
            self.assertFalse(MODULE.complete(repo, expected))

    def test_calibration_only_applies_to_more_ops(self) -> None:
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
                    repo, expected, index=1, count=5, minimum=1.5, maximum=6.0
                ),
                3.0,
            )
            repeated = group(resize_strategy="more-trials", suggested_effective_ops=50_000, suggested_trials=11)
            self.assertIsNone(
                MODULE.calibration_elapsed(
                    repo, repeated, index=1, count=5, minimum=1.5, maximum=6.0
                )
            )

    def test_parse_io_full_avg10(self) -> None:
        sample = "some avg10=1.00 avg60=2.00 avg300=3.00 total=4\nfull avg10=4.75 avg60=2.00 avg300=1.00 total=9\n"
        self.assertEqual(MODULE.parse_io_full_avg10(sample), 4.75)
        with self.assertRaises(ValueError):
            MODULE.parse_io_full_avg10("some avg10=1.00\n")

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
