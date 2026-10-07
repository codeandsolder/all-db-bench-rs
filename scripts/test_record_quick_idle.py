from __future__ import annotations

import importlib.util
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("run-record-quick-idle.py")
SCRUBBER = Path(__file__).with_name("scrub-short-trial-pressure.py")
SPEC = importlib.util.spec_from_file_location("run_record_quick_idle", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class RecordQuickIdleTests(unittest.TestCase):
    def test_parse_io_full_avg10(self) -> None:
        self.assertEqual(MODULE.parse_io_full_avg10("some avg10=1\nfull avg10=4.25 avg60=2 total=3\n"), 4.25)
        with self.assertRaises(ValueError):
            MODULE.parse_io_full_avg10("some avg10=1\n")

    def test_lock_is_exclusive(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            lock = Path(tmp) / "lock"
            first = MODULE.acquire_lock(lock)
            try:
                with self.assertRaises(RuntimeError):
                    MODULE.acquire_lock(lock)
            finally:
                first.close()

    def test_complete_requires_clean_120_row_40_group_summary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            run = repo / "results" / "runs" / "record"
            run.mkdir(parents=True)
            (run / "summary.json").write_text(json.dumps({"row_count": 120, "group_count": 40, "problems": []}))
            self.assertTrue(MODULE.complete(repo, "record"))
            (run / "failures.ndjson").write_text("{}\n")
            self.assertFalse(MODULE.complete(repo, "record"))

    def test_supervisor_scrubs_short_pressure_case(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            scripts = repo / "scripts"
            scripts.mkdir()
            shutil.copy2(SCRUBBER, scripts / SCRUBBER.name)
            run = repo / "results" / "runs" / "record"
            (run / "cases").mkdir(parents=True)
            row = {
                "trial": 1,
                "engine": "sqlite",
                "durability": "relaxed",
                "workload": "write-burst",
                "elapsed_s": 0.05,
                "ops_per_s": 1000.0,
                "measured_process": {
                    "cpu_runtime_fraction_of_wall": 1.0,
                    "runqueue_wait_fraction_of_wall": 0.04,
                },
                "measured_system_delta": {
                    "accounting_wall_ns": 50_000_000,
                    "psi_cpu_some_us": 0,
                },
            }
            case = run / "cases" / "t1-sqlite-relaxed-write-burst.json"
            case.write_text(json.dumps(row))
            self.assertEqual(MODULE.scrub_short_pressure(repo, "record"), 1)
            self.assertFalse(case.exists())
            self.assertEqual(len(list((run / "rejected-pressure").glob("*/attempt-*/rejection.json"))), 1)


if __name__ == "__main__":
    unittest.main()
