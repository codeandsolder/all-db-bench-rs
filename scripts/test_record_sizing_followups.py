from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("run-record-sizing-followups.py")
SPEC = importlib.util.spec_from_file_location("run_record_sizing_followups", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def group(**overrides):
    base = {
        "engine": "sqlite",
        "engine_version": "1",
        "durability": "relaxed",
        "workload": "tiny-txn",
        "records": 10_000,
        "ops_requested": 5_000,
        "suggested_effective_ops": 5_000,
        "runner_ops_override": 10_000,
        "suggested_trials": 11,
        "resize_strategy": "more-trials",
        "median_elapsed_s": 0.1,
        "status": "undersized",
    }
    base.update(overrides)
    return base


class RecordSizingFollowupsTests(unittest.TestCase):
    def test_run_id_encodes_effective_ops_and_trials(self) -> None:
        self.assertEqual(
            MODULE.run_id(group()),
            "20261006-record-resize-v4-sqlite-relaxed-tiny-txn-e5000-t11",
        )

    def test_command_env_uses_record_profile_override_and_trials(self) -> None:
        env = MODULE.command_env(group(), Path("/tmp/recordbench"), Path("/tmp/rocks-recordbench"))
        self.assertEqual(env["RECORD_OPS_OVERRIDE"], "10000")
        self.assertEqual(env["RECORD_TRIALS_OVERRIDE"], "11")
        self.assertEqual(env["RECORD_RECORDS_OVERRIDE"], "10000")
        self.assertEqual(env["WORKLOADS_OVERRIDE"], "tiny-txn")
        self.assertEqual(env["ROCKS_BENCH_BIN"], "/tmp/rocks-recordbench")

    def test_groups_from_plan_appends_quality_repairs(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plan = root / "plan.json"
            quality = root / "quality.json"
            plan.write_text(json.dumps({"sizing_policy_version": 2, "groups": []}))
            repair = group(engine="sqlite", workload="write-burst", resize_strategy="quality-repair", suggested_trials=3)
            repair["status"] = "variable"
            quality.write_text(json.dumps({"quality_policy_version": 1, "groups": [repair]}))
            groups = MODULE.groups_from_plan(plan, quality)
        self.assertEqual(len(groups), 1)
        self.assertEqual(groups[0]["resize_strategy"], "quality-repair")

    def test_groups_from_plan_puts_more_ops_first(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            plan = Path(tmp) / "plan.json"
            plan.write_text(
                json.dumps(
                    {
                        "sizing_policy_version": 2,
                        "groups": [
                            group(engine="repeat", median_elapsed_s=0.01),
                            group(
                                engine="read",
                                workload="point-read",
                                resize_strategy="more-ops",
                                suggested_trials=3,
                                suggested_effective_ops=50_000,
                                runner_ops_override=50_000,
                                median_elapsed_s=1.0,
                            ),
                            group(engine="ok", status="accepted"),
                        ],
                    }
                )
            )
            groups = MODULE.groups_from_plan(plan)
            self.assertEqual([item["engine"] for item in groups], ["read", "repeat"])

    def test_calibration_skips_more_trials(self) -> None:
        self.assertIsNone(
            MODULE.calibration_elapsed(
                Path("/does-not-matter"),
                group(),
                index=1,
                count=5,
                minimum=1.5,
                maximum=15.0,
            )
        )

    def test_parse_io_full_avg10(self) -> None:
        self.assertEqual(MODULE.parse_io_full_avg10("full avg10=3.25 avg60=1 total=2\n"), 3.25)


if __name__ == "__main__":
    unittest.main()
