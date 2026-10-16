from __future__ import annotations

import hashlib
import importlib.util
import json
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path

SCRIPT = Path(__file__).with_name("run-reopen-idle-queue.py")
SPEC = importlib.util.spec_from_file_location("run_reopen_idle_queue", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class ReopenIdleQueueTests(unittest.TestCase):
    def test_parse_io_full_avg10(self) -> None:
        self.assertEqual(MODULE.parse_io_full_avg10("some avg10=1\nfull avg10=4.25 avg60=1\n"), 4.25)
        with self.assertRaises(ValueError):
            MODULE.parse_io_full_avg10("some avg10=1\n")

    def test_load_and_verify_binary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / "kv"
            binary.write_bytes(b"kv")
            binary.chmod(0o755)
            manifest = {"repo_commit": "a" * 40, "kv": {"path": str(binary), "sha256": hashlib.sha256(b"kv").hexdigest()}}
            path = root / "manifest.json"
            path.write_text(json.dumps(manifest))
            self.assertEqual(MODULE.load_and_verify_binary(path)["repo_commit"], "a" * 40)
            manifest["kv"]["sha256"] = "bad"
            path.write_text(json.dumps(manifest))
            with self.assertRaises(RuntimeError):
                MODULE.load_and_verify_binary(path)

    def test_run_complete_requires_exact_150_rows_50_groups(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            run = repo / "results" / "runs" / "r"
            cases = run / "cases"
            cases.mkdir(parents=True)
            (run / "support.json").write_text(json.dumps({"case_count": 150, "trials": 3}))
            (run / "summary.json").write_text(json.dumps({"row_count": 150, "group_count": 50, "problems": []}))
            for i in range(150):
                (cases / f"{i}.json").write_text("{}")
            self.assertTrue(MODULE.run_complete(repo, "r"))
            (cases / "149.json").unlink()
            self.assertFalse(MODULE.run_complete(repo, "r"))

    def test_complete_run_is_rejected_when_runtime_provenance_mismatches(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp); run = repo / "results" / "runs" / "r"; cases = run / "cases"; cases.mkdir(parents=True)
            (run / "support.json").write_text(json.dumps({"case_count": 150, "trials": 3}))
            (run / "summary.json").write_text(json.dumps({"row_count": 150, "group_count": 50, "problems": []}))
            for i in range(150): (cases / f"{i}.json").write_text("{}")
            with mock.patch.object(MODULE, "support_matches_runtime", return_value=False):
                self.assertFalse(MODULE.run_complete(repo, "r", {}))
            with mock.patch.object(MODULE, "support_matches_runtime", return_value=True):
                self.assertTrue(MODULE.run_complete(repo, "r", {}))

    def test_stage_env_passes_verified_hash_and_resume_policy(self) -> None:
        manifest = {"repo_commit": "a" * 40, "kv": {"path": "/kv", "sha256": "k"}}
        env = MODULE.stage_env(Path("/repo"), manifest)
        self.assertEqual(env["BENCH_BIN"], "/kv")
        self.assertEqual(env["BENCH_BIN_SHA256"], "k")
        self.assertEqual(env["MATRIX_RESUME_SHUFFLE_REMAINING"], "1")
        self.assertEqual(env["RUN_ID"], MODULE.RUN_ID)
        self.assertEqual(env["BENCH_SOURCE_COMMIT"], "a" * 40)


if __name__ == "__main__":
    unittest.main()
