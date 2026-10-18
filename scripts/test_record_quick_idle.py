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

    def test_audit_runs_non_executable_script_through_uv(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            (repo / "results" / "runs" / "record").mkdir(parents=True)
            audit_dir = repo / "audit"
            original_run = MODULE.subprocess.run
            calls = []

            class Result:
                returncode = 0

            def fake_run(argv, **kwargs):
                calls.append((argv, kwargs))
                return Result()

            MODULE.subprocess.run = fake_run
            try:
                MODULE.audit(repo, "record", audit_dir)
            finally:
                MODULE.subprocess.run = original_run

            self.assertEqual(calls[0][0][:4], ["uv", "run", "--script", str(repo / "scripts" / "audit_baseline_sizing.py")])
            self.assertEqual(calls[0][1]["cwd"], repo)

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


if __name__ == "__main__":
    unittest.main()
