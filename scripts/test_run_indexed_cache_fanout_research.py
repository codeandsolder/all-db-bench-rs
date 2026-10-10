from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("run-indexed-cache-fanout-research.py")
SPEC = importlib.util.spec_from_file_location("run_indexed_cache_fanout_research", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


class CacheFanoutRunnerTests(unittest.TestCase):
    def test_common_ops_targets_fastest_with_slowest_ceiling(self) -> None:
        self.assertEqual(
            M.choose_common_ops({"sqlite": 1000.0, "turso": 500.0}, target_s=3.0, ceiling_s=15.0),
            3000,
        )

    def test_common_ops_respects_slowest_ceiling(self) -> None:
        self.assertEqual(
            M.choose_common_ops({"fast": 10000.0, "slow": 100.0}, target_s=3.0, ceiling_s=15.0),
            1500,
        )

    def test_cell_identity_distinguishes_default_and_explicit_cache(self) -> None:
        self.assertNotEqual(
            M.cell_slug("sqlite", None, 100),
            M.cell_slug("sqlite", 2000, 100),
        )

    def test_validate_row_accepts_explicit_cache_roundtrip(self) -> None:
        row = {
            "engine": "sqlite", "durability": "sync", "workload": "indexed-read",
            "indexed_read_limit": 100, "sql_cache_kib": 2000, "sql_cache_pragma_value": -2000,
            "ops_requested": 1000, "ops_completed": 1000, "trial": 1,
            "read_materialization": M.READ_MATERIALIZATION,
            "write_materialization": M.WRITE_MATERIALIZATION,
        }
        M.validate_row(row, engine="sqlite", cache_kib=2000, limit=100, ops=1000, trial=1)

    def test_validate_row_rejects_cache_identity_drift(self) -> None:
        row = {
            "engine": "sqlite", "durability": "sync", "workload": "indexed-read",
            "indexed_read_limit": 100, "sql_cache_kib": 8000,
            "sql_cache_pragma_value": -8000,
            "ops_requested": 1000, "ops_completed": 1000, "trial": 1,
            "read_materialization": M.READ_MATERIALIZATION,
            "write_materialization": M.WRITE_MATERIALIZATION,
        }
        with self.assertRaisesRegex(RuntimeError, "identity mismatch"):
            M.validate_row(row, engine="sqlite", cache_kib=2000, limit=100, ops=1000, trial=1)


if __name__ == "__main__":
    unittest.main()
