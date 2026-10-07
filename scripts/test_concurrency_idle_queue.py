from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("run-concurrency-idle-queue.py")
SPEC = importlib.util.spec_from_file_location("run_concurrency_idle_queue", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class ConcurrencyIdleQueueTests(unittest.TestCase):
    def test_parse_io_full_avg10(self) -> None:
        self.assertEqual(MODULE.parse_io_full_avg10("some avg10=1.0\nfull avg10=4.25 avg60=1\n"), 4.25)
        with self.assertRaises(ValueError):
            MODULE.parse_io_full_avg10("some avg10=1.0\n")

    def test_load_and_verify_binaries(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = {"repo_commit": "abc"}
            for key in ("kv", "record", "rocks"):
                path = root / key
                path.write_bytes(key.encode())
                path.chmod(0o755)
                manifest[key] = {"path": str(path), "sha256": hashlib.sha256(key.encode()).hexdigest()}
            manifest_path = root / "manifest.json"
            manifest_path.write_text(json.dumps(manifest))
            self.assertEqual(MODULE.load_and_verify_binaries(manifest_path)["repo_commit"], "abc")
            manifest["kv"]["sha256"] = "bad"
            manifest_path.write_text(json.dumps(manifest))
            with self.assertRaises(RuntimeError):
                MODULE.load_and_verify_binaries(manifest_path)

    def test_run_complete_checks_support_summary_and_failures(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            run = repo / "results" / "runs" / "r"
            run.mkdir(parents=True)
            (run / "support.json").write_text(json.dumps({"case_count": 12, "trials": 3}))
            (run / "summary.json").write_text(json.dumps({"row_count": 12, "group_count": 4, "problems": []}))
            self.assertTrue(MODULE.run_complete(repo, "r"))
            (run / "failures.ndjson").write_text("{}\n")
            self.assertFalse(MODULE.run_complete(repo, "r"))

    def test_stage_env_passes_verified_hashes(self) -> None:
        binaries = {
            "kv": {"path": "/kv", "sha256": "k"},
            "record": {"path": "/record", "sha256": "r"},
            "rocks": {"path": "/rocks", "sha256": "x"},
        }
        kv = MODULE.stage_env(Path("/repo"), binaries, lane="kv", run_id="kv-run")
        self.assertEqual(kv["BENCH_BIN_SHA256"], "k")
        self.assertEqual(kv["MATRIX_RESUME_SHUFFLE_REMAINING"], "1")
        record = MODULE.stage_env(Path("/repo"), binaries, lane="record", run_id="record-run")
        self.assertEqual(record["BENCH_BIN_SHA256"], "r")
        self.assertEqual(record["ROCKS_BENCH_BIN_SHA256"], "x")


if __name__ == "__main__":
    unittest.main()
