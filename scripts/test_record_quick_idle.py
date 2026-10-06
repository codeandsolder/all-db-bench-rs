from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("run-record-quick-idle.py")
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

    def test_complete_requires_120_rows_and_40_groups(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            run = repo / "results" / "runs" / "r"
            run.mkdir(parents=True)
            (run / "summary.json").write_text(json.dumps({"row_count": 120, "group_count": 40, "problems": []}))
            self.assertTrue(MODULE.complete(repo, "r"))
            (run / "failures.ndjson").write_text("failure\n")
            self.assertFalse(MODULE.complete(repo, "r"))

    def test_lock_is_exclusive(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            lock = Path(tmp) / "lock"
            first = MODULE.acquire_lock(lock)
            try:
                with self.assertRaises(RuntimeError):
                    MODULE.acquire_lock(lock)
            finally:
                first.close()


if __name__ == "__main__":
    unittest.main()
