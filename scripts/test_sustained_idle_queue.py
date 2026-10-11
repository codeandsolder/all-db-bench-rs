from __future__ import annotations

import hashlib
import importlib.util
import json
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path

SCRIPT = Path(__file__).with_name("run-sustained-idle-queue.py")
SPEC = importlib.util.spec_from_file_location("run_sustained_idle_queue", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class SustainedIdleQueueTests(unittest.TestCase):
    def test_parse_io_full_avg10(self) -> None:
        self.assertEqual(MODULE.parse_io_full_avg10("some avg10=1.0\nfull avg10=4.25 avg60=1\n"), 4.25)
        with self.assertRaises(ValueError):
            MODULE.parse_io_full_avg10("some avg10=1.0\n")

    def test_load_and_verify_binaries(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = {"repo_commit": "mixed", "source_commits": {"kv": "a" * 40, "record": "b" * 40, "record_rocksdb": "c" * 40}, "read_materialization": "full-record-v1", "write_materialization": "no-return-v1"}
            for key in ("kv", "record", "record_rocksdb"):
                path = root / key
                path.write_bytes(key.encode())
                path.chmod(0o755)
                manifest[key] = {"path": str(path), "sha256": hashlib.sha256(key.encode()).hexdigest()}
            manifest_path = root / "manifest.json"
            manifest_path.write_text(json.dumps(manifest))
            self.assertEqual(MODULE.load_and_verify_binaries(manifest_path)["repo_commit"], "mixed")
            manifest["kv"]["sha256"] = "bad"
            manifest_path.write_text(json.dumps(manifest))
            with self.assertRaises(RuntimeError):
                MODULE.load_and_verify_binaries(manifest_path)

    def test_run_complete_requires_exact_cases_and_groups(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            run = repo / "results" / "runs" / "r"
            cases = run / "cases"
            cases.mkdir(parents=True)
            (run / "support.json").write_text(json.dumps({"case_count": 12, "trials": 3}))
            (run / "summary.json").write_text(json.dumps({"row_count": 12, "group_count": 4, "problems": []}))
            for i in range(12):
                (cases / f"{i}.json").write_text("{}")
            self.assertTrue(MODULE.run_complete(repo, "r"))
            (cases / "11.json").unlink()
            self.assertFalse(MODULE.run_complete(repo, "r"))

    def test_complete_run_is_rejected_when_runtime_provenance_mismatches(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp); run = repo / "results" / "runs" / "r"; cases = run / "cases"; cases.mkdir(parents=True)
            (run / "support.json").write_text(json.dumps({"case_count": 12, "trials": 3}))
            (run / "summary.json").write_text(json.dumps({"row_count": 12, "group_count": 4, "problems": []}))
            for i in range(12): (cases / f"{i}.json").write_text("{}")
            with mock.patch.object(MODULE, "support_matches_runtime", return_value=False):
                self.assertFalse(MODULE.run_complete(repo, "r", {}, "kv"))
            with mock.patch.object(MODULE, "support_matches_runtime", return_value=True):
                self.assertTrue(MODULE.run_complete(repo, "r", {}, "kv"))

    def test_stage_env_passes_verified_hash(self) -> None:
        binaries = {
            "kv": {"path": "/kv", "sha256": "k"},
            "record": {"path": "/record", "sha256": "r"},
            "record_rocksdb": {"path": "/record-rocksdb", "sha256": "rr"},
            "source_commits": {"kv": "a" * 40, "record": "b" * 40, "record_rocksdb": "c" * 40},
        }
        kv = MODULE.stage_env(Path("/repo"), binaries, lane="kv", run_id="kv-run")
        self.assertEqual(kv["BENCH_BIN_SHA256"], "k")
        self.assertEqual(kv["MATRIX_RESUME_SHUFFLE_REMAINING"], "1")
        self.assertEqual(kv["BENCH_SOURCE_COMMIT"], "a" * 40)
        record = MODULE.stage_env(Path("/repo"), binaries, lane="record", run_id="record-run")
        self.assertEqual(record["BENCH_BIN_SHA256"], "r")
        self.assertEqual(record["BENCH_ROCKS_BIN"], "/record-rocksdb")
        self.assertEqual(record["BENCH_ROCKS_BIN_SHA256"], "rr")
        self.assertEqual(record["BENCH_SOURCE_COMMIT"], "b" * 40)
        self.assertEqual(record["ROCKS_BENCH_SOURCE_COMMIT"], "c" * 40)


if __name__ == "__main__":
    unittest.main()
