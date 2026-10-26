from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).with_name("run-baseline-generation.py")
SPEC = importlib.util.spec_from_file_location("run_baseline_generation", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


SCRIPT_NAMES = (
    "run-kv-quick-idle.py",
    "run-record-quick-idle.py",
    "audit-stock-short-pressure.py",
    "run-kv-sizing-followups.py",
    "run-record-sizing-followups.py",
    "benchctl.sh",
    "verify-kv-ready.py",
    "verify-record-ready.py",
)


class BaselineGenerationTests(unittest.TestCase):
    def make_repo(self, root: Path) -> Path:
        repo = root / "repo"
        scripts = repo / "scripts"
        scripts.mkdir(parents=True)
        for name in SCRIPT_NAMES:
            path = scripts / name
            path.write_text("#!/bin/sh\nexit 0\n")
            path.chmod(0o755)
        return repo

    def test_generation_paths_have_one_namespace(self) -> None:
        paths = MODULE.generation_paths(Path("/generation"), "20261016-refresh")
        self.assertEqual(paths.kv_stock_id, "20261016-refresh-kv-stock")
        self.assertEqual(paths.record_stock_id, "20261016-refresh-record-stock")
        self.assertEqual(paths.kv_resize_prefix, "20261016-refresh-kv-resize")
        self.assertEqual(paths.record_resize_prefix, "20261016-refresh-record-resize")
        self.assertEqual(paths.kv_audit, Path("/generation/plans/20261016-refresh-kv-sizing.json"))

    def test_load_pins_requires_one_commit_without_eager_binary_hashing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = self.make_repo(root)
            commit = "b" * 40
            kvbench = root / "kvbench"
            record = root / "recordbench"
            rocks = root / "rocks-recordbench"
            for binary, payload in ((kvbench, b"kv"), (record, b"record"), (rocks, b"rocks")):
                binary.write_bytes(payload)
                binary.chmod(0o755)
            kv_manifest = root / "kv.json"
            kv_manifest.write_text(json.dumps({"repo_commit": commit, "kv": {"path": str(kvbench), "sha256": MODULE.sha256(kvbench)}}))
            record_manifest = root / "record.json"
            record_manifest.write_text(json.dumps({"repo_commit": commit, "binaries": {
                "recordbench": {"path": str(record), "sha256": MODULE.sha256(record)},
                "surrealdb-rocksdb-recordbench": {"path": str(rocks), "sha256": MODULE.sha256(rocks)},
            }}))
            with (
                mock.patch.object(MODULE, "checked_repo_commit", return_value=commit),
                mock.patch.object(MODULE, "sha256", side_effect=AssertionError("binary hash must be deferred")),
            ):
                pins = MODULE.load_pins(repo, kv_manifest, record_manifest)
            self.assertEqual(pins.commit, commit)
            self.assertEqual(pins.kvbench, kvbench)
            with mock.patch.object(MODULE, "checked_repo_commit", return_value="c" * 40):
                with self.assertRaisesRegex(ValueError, "one exact repo commit"):
                    MODULE.load_pins(repo, kv_manifest, record_manifest)


    def test_load_pins_rejects_malformed_manifest_hash_without_reading_binary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = self.make_repo(root)
            binary = root / "kvbench"
            binary.write_bytes(b"kv")
            binary.chmod(0o755)
            record = root / "recordbench"
            rocks = root / "rocks"
            for path in (record, rocks):
                path.write_bytes(b"x")
                path.chmod(0o755)
            commit = "b" * 40
            kv_manifest = root / "kv.json"
            kv_manifest.write_text(json.dumps({"repo_commit": commit, "kv": {"path": str(binary), "sha256": "bad"}}))
            record_manifest = root / "record.json"
            record_manifest.write_text(json.dumps({"repo_commit": commit, "binaries": {
                "recordbench": {"path": str(record), "sha256": "1" * 64},
                "surrealdb-rocksdb-recordbench": {"path": str(rocks), "sha256": "2" * 64},
            }}))
            with mock.patch.object(MODULE, "checked_repo_commit", return_value=commit):
                with self.assertRaisesRegex(ValueError, "manifest SHA-256 is invalid"):
                    MODULE.load_pins(repo, kv_manifest, record_manifest)

    def test_stage_plan_is_generation_scoped_and_uses_continuous_ready_verifiers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = self.make_repo(root)
            generation_root = root / "generation"
            paths = MODULE.generation_paths(generation_root, "g")
            pins = MODULE.Pins(
                commit="d" * 40,
                kvbench=root / "kvbench",
                kvbench_sha256="kvsha",
                recordbench=root / "recordbench",
                recordbench_sha256="recordsha",
                rocks_recordbench=root / "rocks",
                rocks_recordbench_sha256="rockssha",
            )
            planned = MODULE.stages(
                repo, paths, pins, root / "kv-manifest.json", root / "record-manifest.json",
                Path("/run/lock/all-db-bench-performance.lock"), watch=True, busy_sleep=3.0,
            )
            names = [name for name, _ in planned]
            self.assertEqual(names[0:2], ["kv-stock", "record-stock"])
            self.assertEqual(names[-3:], ["finalize", "verify-kv", "verify-record"])
            self.assertEqual(len(names), 13)
            all_args = "\n".join(" ".join(argv) for _, argv in planned)
            self.assertIn("g-kv-stock", all_args)
            self.assertIn("g-record-stock", all_args)
            self.assertIn("g-kv-resize", all_args)
            self.assertIn("g-record-resize", all_args)
            self.assertIn(str(generation_root / "ready" / "kv.json"), all_args)
            self.assertIn(str(generation_root / "ready" / "record.json"), all_args)
            kv_stock = dict(planned)["kv-stock"]
            self.assertIn("--watch", kv_stock)
            self.assertIn("--audit-json", kv_stock)
            self.assertIn(str(paths.kv_audit), kv_stock)


if __name__ == "__main__":
    unittest.main()
