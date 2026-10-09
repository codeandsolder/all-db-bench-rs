from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("summarize.py")
SPEC = importlib.util.spec_from_file_location("summarize_record_read_identity", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


def row() -> dict:
    return {
        "format_version": 7,
        "lane": "record",
        "scenario": "baseline-core",
        "engine": "sqlite",
        "engine_version": "rusqlite 0.40.2 / SQLite 3.53.4",
        "durability": "sync",
        "workload": "point-read",
        "records": 10_000,
        "ops_requested": 10_000,
        "payload_bytes": 512,
        "txn_size": 100,
        "read_materialization": "full-record-v1",
        "write_materialization": "no-return-v1",
    }


class SummarizeRecordReadIdentityTests(unittest.TestCase):
    def test_read_materialization_is_identity(self) -> None:
        corrected = row()
        legacy = dict(corrected, read_materialization="legacy-read-v0")
        self.assertNotEqual(M.group_key(corrected), M.group_key(legacy))

    def test_missing_read_marker_defaults_to_legacy(self) -> None:
        missing = row()
        missing.pop("read_materialization")
        explicit = dict(row(), read_materialization="legacy-read-v0")
        self.assertEqual(M.group_key(missing), M.group_key(explicit))

    def test_write_materialization_is_identity(self) -> None:
        corrected = row()
        legacy = dict(corrected, write_materialization="legacy-return-v0")
        self.assertNotEqual(M.group_key(corrected), M.group_key(legacy))

    def test_missing_write_marker_defaults_to_legacy(self) -> None:
        missing = row()
        missing.pop("write_materialization")
        explicit = dict(row(), write_materialization="legacy-return-v0")
        self.assertEqual(M.group_key(missing), M.group_key(explicit))


if __name__ == "__main__":
    unittest.main()
