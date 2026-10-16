from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("summarize.py")
SPEC = importlib.util.spec_from_file_location("summarize_identity", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC); sys.modules[SPEC.name] = M; SPEC.loader.exec_module(M)


def row() -> dict:
    return {
        "format_version": 6, "lane": "kv-concurrency", "scenario": "concurrency-primary",
        "engine": "redb", "engine_version": "4.3.0", "durability": "relaxed", "workload": "balanced",
        "records": 100_000, "ops_requested": 50_000, "clients": 8, "value_bytes": 256,
        "value_pattern": "pseudo-random", "key_bytes": 8, "key_shape": "sequential",
        "access_pattern": "auto", "miss_percent": 0, "write_pattern": "update-uniform",
        "state_evolution": "growth", "bounded_churn_slots": 0, "txn_size": 100, "scan_len": 100,
    }


class SummarizeConcurrencyIdentityTests(unittest.TestCase):
    def test_state_evolution_and_pool_are_identity(self) -> None:
        growth = row()
        bounded = dict(growth, state_evolution="bounded", bounded_churn_slots=8192)
        self.assertNotEqual(M.group_key(growth), M.group_key(bounded))

    def test_legacy_defaults_are_growth_without_pool(self) -> None:
        legacy = row(); legacy.pop("state_evolution"); legacy.pop("bounded_churn_slots")
        explicit = row()
        self.assertEqual(M.group_key(legacy), M.group_key(explicit))

    def test_read_materialization_is_concurrency_identity(self) -> None:
        corrected = dict(row(), read_materialization="full-record-v1")
        legacy = dict(row(), read_materialization="legacy-read-v0")
        corrected["settle_ms"] = 0
        legacy["settle_ms"] = 0
        self.assertNotEqual(M.group_key(corrected), M.group_key(legacy))
        self.assertNotEqual(M.concurrency_identity(corrected), M.concurrency_identity(legacy))

    def test_write_materialization_is_concurrency_identity(self) -> None:
        corrected = dict(row(), write_materialization="no-return-v1")
        legacy = dict(row(), write_materialization="legacy-return-v0")
        corrected["settle_ms"] = 0
        legacy["settle_ms"] = 0
        self.assertNotEqual(M.group_key(corrected), M.group_key(legacy))
        self.assertNotEqual(M.concurrency_identity(corrected), M.concurrency_identity(legacy))

    def test_c1_baseline_identity_separates_state(self) -> None:
        growth = row(); growth["settle_ms"] = 0
        bounded = dict(growth, state_evolution="bounded", bounded_churn_slots=8192)
        self.assertNotEqual(M.concurrency_identity(growth), M.concurrency_identity(bounded))


if __name__ == "__main__": unittest.main()
