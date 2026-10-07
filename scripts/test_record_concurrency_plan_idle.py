from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("run-record-concurrency-plan-idle.py")
SPEC = importlib.util.spec_from_file_location("record_idle", SCRIPT)
assert SPEC and SPEC.loader
M = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(M)


class RecordConcurrencyIdleTests(unittest.TestCase):
    def test_run_complete_accepts_exact_clean_summary(self):
        with tempfile.TemporaryDirectory() as td:
            repo = Path(td)
            run = repo / "results" / "runs" / "x"
            run.mkdir(parents=True)
            (run / "support.json").write_text(json.dumps({"case_count": 12, "expect_trials": 3}))
            (run / "summary.json").write_text(json.dumps({"row_count": 12, "group_count": 4, "problems": []}))
            self.assertTrue(M.run_complete(repo, "x"))

    def test_run_complete_rejects_failure_file(self):
        with tempfile.TemporaryDirectory() as td:
            repo = Path(td)
            run = repo / "results" / "runs" / "x"
            run.mkdir(parents=True)
            (run / "support.json").write_text(json.dumps({"case_count": 3, "expect_trials": 1}))
            (run / "summary.json").write_text(json.dumps({"row_count": 3, "group_count": 3, "problems": []}))
            (run / "failures.ndjson").write_text("{}\n")
            self.assertFalse(M.run_complete(repo, "x"))


if __name__ == "__main__":
    unittest.main()
