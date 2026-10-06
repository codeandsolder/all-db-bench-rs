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
        "suggested_effective_ops": 30_000,
        "runner_ops_override": 60_000,
        "median_elapsed_s": 0.5,
        "status": "undersized",
    }
    base.update(overrides)
    return base


class RecordSizingFollowupsTests(unittest.TestCase):
    def test_run_id_encodes_effective_ops(self) -> None:
        self.assertEqual(
            MODULE.run_id(group()),
            "20261006-record-resize-sqlite-relaxed-tiny-txn-e30000",
        )

    def test_command_env_uses_record_profile_override(self) -> None:
        env = MODULE.command_env(group(), Path("/tmp/recordbench"), Path("/tmp/rocks-recordbench"))
        self.assertEqual(env["RECORD_OPS_OVERRIDE"], "60000")
        self.assertEqual(env["RECORD_RECORDS_OVERRIDE"], "10000")
        self.assertEqual(env["WORKLOADS_OVERRIDE"], "tiny-txn")
        self.assertEqual(env["ROCKS_BENCH_BIN"], "/tmp/rocks-recordbench")

    def test_groups_from_plan_filters_and_sorts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            plan = Path(tmp) / "plan.json"
            plan.write_text(json.dumps({"groups": [group(engine="slow", median_elapsed_s=1.0), group(engine="fast", median_elapsed_s=0.1), group(engine="ok", status="accepted")]}))
            groups = MODULE.groups_from_plan(plan)
            self.assertEqual([item["engine"] for item in groups], ["fast", "slow"])

    def test_parse_io_full_avg10(self) -> None:
        self.assertEqual(MODULE.parse_io_full_avg10("full avg10=3.25 avg60=1 total=2\n"), 3.25)


if __name__ == "__main__":
    unittest.main()
