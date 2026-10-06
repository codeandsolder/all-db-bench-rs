
from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("select-baseline-sizing-results.py")
SPEC = importlib.util.spec_from_file_location("select_baseline_sizing_results_quality", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def row(workload: str, trial: int, *, elapsed: float, rate: float) -> dict[str, object]:
    return {
        "lane": "kv",
        "scenario": "baseline-core",
        "engine": "x",
        "engine_version": "1",
        "durability": "relaxed",
        "workload": workload,
        "records": 100,
        "ops_requested": 100,
        "clients": 1,
        "value_bytes": 256,
        "value_pattern": "pseudo-random",
        "key_bytes": 8,
        "key_shape": "sequential",
        "access_pattern": "auto",
        "miss_percent": 0,
        "write_pattern": "append",
        "txn_size": 100,
        "scan_len": 100,
        "settle_ms": 0,
        "configuration": "fixed",
        "trial": trial,
        "elapsed_s": elapsed,
        "ops_per_s": rate,
    }


THRESHOLDS = {
    "expect_trials": 3,
    "max_cv": 0.10,
    "max_relative_spread": 0.25,
    "read_only_min_seconds": 0.5,
    "read_only_workloads": ["indexed-read", "point-read", "range-scan"],
    "stateful_min_total_seconds": 1.0,
}


class FinalQualityTests(unittest.TestCase):
    def test_stateful_reclassifies_variable_after_sampling(self) -> None:
        rows = [row("tiny-txn", trial, elapsed=0.2, rate=rate) for trial, rate in enumerate((100, 100, 100, 100, 40), 1)]
        quality = MODULE.final_quality(rows, THRESHOLDS)
        self.assertEqual(quality["final_status"], "variable")
        self.assertGreater(quality["throughput_relative_spread"], 0.25)

    def test_read_only_remains_undersized_when_resize_misses_floor(self) -> None:
        rows = [row("point-read", trial, elapsed=0.2, rate=100) for trial in (1, 2, 3)]
        quality = MODULE.final_quality(rows, THRESHOLDS)
        self.assertEqual(quality["final_status"], "undersized")

    def test_realistic_stateful_total_time_can_be_accepted(self) -> None:
        rows = [row("write-burst", trial, elapsed=0.21, rate=100) for trial in range(1, 6)]
        quality = MODULE.final_quality(rows, THRESHOLDS)
        self.assertEqual(quality["final_status"], "accepted")
        self.assertGreaterEqual(quality["total_elapsed_s"], 1.0)


if __name__ == "__main__":
    unittest.main()
