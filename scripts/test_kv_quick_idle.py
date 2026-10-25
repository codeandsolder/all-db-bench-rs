from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("run-kv-quick-idle.py")
SPEC = importlib.util.spec_from_file_location("run_kv_quick_idle", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class KvQuickIdleTests(unittest.TestCase):
    def test_parse_io_full_avg10(self) -> None:
        self.assertEqual(MODULE.parse_io_full_avg10("some avg10=1\nfull avg10=4.25 avg60=2 total=3\n"), 4.25)
        with self.assertRaises(ValueError):
            MODULE.parse_io_full_avg10("some avg10=1\n")

    def test_audit_runs_native_benchctl(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            (repo / "results" / "runs" / "kv").mkdir(parents=True)
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
                MODULE.audit(repo, "kv", audit_dir)
            finally:
                MODULE.subprocess.run = original_run

            self.assertEqual(calls[0][0][:2], [str(repo / "scripts" / "benchctl.sh"), "audit-baseline-sizing"])
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

    def test_complete_requires_clean_540_row_180_group_summary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            run = repo / "results" / "runs" / "kv"
            run.mkdir(parents=True)
            (run / "summary.json").write_text(json.dumps({"row_count": 540, "group_count": 180, "problems": []}))
            self.assertTrue(MODULE.complete(repo, "kv"))
            (run / "failures.ndjson").write_text("{}\n")
            self.assertFalse(MODULE.complete(repo, "kv"))


if __name__ == "__main__":
    unittest.main()
