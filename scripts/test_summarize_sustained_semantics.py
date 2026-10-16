from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("summarize-sustained.py")
SPEC = importlib.util.spec_from_file_location("summarize_sustained_semantics", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def case() -> dict:
    return {
        "format_version": 7, "lane": "record-sustained", "scenario": "record-sustained",
        "engine": "sqlite", "engine_version": "1", "durability": "sync",
        "durability_mapping": "full", "pattern": "append", "records": 100,
        "ops_requested": 1000, "window_ops": 100, "value_bytes": None,
        "payload_bytes": 512, "value_pattern": None, "key_bytes": None, "key_shape": None,
        "txn_size": 100, "read_materialization": "full-record-v1",
        "write_materialization": "no-return-v1",
    }


class SustainedSemanticIdentityTests(unittest.TestCase):
    def test_read_and_write_materialization_are_group_identity(self) -> None:
        corrected = case()
        old_read = dict(corrected, read_materialization="legacy-read-v0")
        old_write = dict(corrected, write_materialization="legacy-return-v0")
        self.assertNotEqual(MODULE.group_key(corrected), MODULE.group_key(old_read))
        self.assertNotEqual(MODULE.group_key(corrected), MODULE.group_key(old_write))

    def test_missing_markers_default_to_legacy(self) -> None:
        missing = case()
        missing.pop("read_materialization")
        missing.pop("write_materialization")
        legacy = dict(
            case(),
            read_materialization="legacy-read-v0",
            write_materialization="legacy-return-v0",
        )
        self.assertEqual(MODULE.group_key(missing), MODULE.group_key(legacy))


if __name__ == "__main__":
    unittest.main()
