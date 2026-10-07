from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("run-kv-concurrency-plan-idle.py")
SPEC = importlib.util.spec_from_file_location("run_kv_concurrency_plan_idle", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC); sys.modules[SPEC.name] = M; SPEC.loader.exec_module(M)


class PlanIdleTests(unittest.TestCase):
    def test_parse_io_full_avg10(self) -> None:
        self.assertEqual(M.parse_io_full_avg10("some avg10=1\nfull avg10=4.25 avg60=1\n"), 4.25)
        with self.assertRaises(ValueError): M.parse_io_full_avg10("some avg10=1\n")

    def test_verify_binary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/"bin"; path.write_bytes(b"abc"); path.chmod(0o755)
            sha=hashlib.sha256(b"abc").hexdigest(); M.verify_binary(path,sha)
            with self.assertRaisesRegex(RuntimeError,"mismatch"): M.verify_binary(path,"bad")

    def test_run_complete(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo=Path(tmp); run=repo/"results"/"runs"/"r"; run.mkdir(parents=True)
            (run/"support.json").write_text(json.dumps({"case_count":2,"expect_trials":1}))
            (run/"summary.json").write_text(json.dumps({"row_count":2,"group_count":2,"problems":[]}))
            self.assertTrue(M.run_complete(repo,"r"))
            (run/"failures.ndjson").write_text("{}\n")
            self.assertFalse(M.run_complete(repo,"r"))


if __name__ == "__main__": unittest.main()
