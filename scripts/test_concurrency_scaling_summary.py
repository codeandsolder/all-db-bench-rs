from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("summarize-concurrency-scaling.py")
SPEC = importlib.util.spec_from_file_location("summarize_concurrency_scaling", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def group(clients: int, rate: float, *, state_evolution: str = "growth", bounded_churn_slots: int = 0, format_version: int = 5) -> dict:
    speed = rate / 100.0
    return {
        "format_version": format_version, "lane": "kv-concurrency", "scenario": "concurrency-primary",
        "engine": "x", "engine_version": "1", "durability": "sync", "workload": "point-read",
        "records": 100, "ops_requested": 1000, "clients": clients, "value_bytes": 256,
        "value_pattern": "pseudo-random", "key_bytes": 8, "key_shape": "sequential",
        "access_pattern": "auto", "miss_percent": 0, "write_pattern": "append",
        "state_evolution": state_evolution, "bounded_churn_slots": bounded_churn_slots,
        "txn_size": 100, "scan_len": 100, "settle_ms": 0, "ops_per_s_median": rate,
        "speedup_vs_c1": speed, "parallel_efficiency_vs_c1": speed / clients,
        "p99_read_multiplier_vs_c1": float(clients), "p99_write_txn_multiplier_vs_c1": None,
        "client_throughput_max_min_ratio_median": 1.1, "cpu_cores_median": float(clients),
        "write_conflict_retries_per_k_write_ops_median": 0.0,
    }


class ScalingSummaryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.support = {
            "lane": "kv-concurrency", "profile": "quick", "trials": 3, "case_count": 12,
            "clients": "1 2 4 8", "range_clients": "1 4 8", "delete_clients": "1 4 8",
            "relaxed_clients": "1 4 8",
        }

    def test_complete_family_uses_max_client_metrics(self) -> None:
        summary = {"row_count": 12, "group_count": 4, "problems": [], "groups": [
            group(1, 100), group(2, 180), group(4, 300), group(8, 400),
        ]}
        report = MODULE.build_report(summary, self.support)
        self.assertEqual(report["problems"], [])
        self.assertEqual(report["complete_family_count"], 1)
        family = report["families"][0]
        self.assertEqual(family["observed_clients"], [1, 2, 4, 8])
        self.assertEqual(family["speedup_at_max_clients"], 4.0)
        self.assertEqual(family["parallel_efficiency_at_max_clients"], 0.5)

    def test_state_evolution_is_part_of_family_identity(self) -> None:
        growth = [group(c, r, state_evolution="growth", format_version=6) for c, r in ((1,100),(2,180),(4,300),(8,400))]
        bounded = [group(c, r, state_evolution="bounded", bounded_churn_slots=8192, format_version=6) for c, r in ((1,90),(2,170),(4,290),(8,390))]
        support = dict(self.support); support["case_count"] = 24
        summary = {"row_count": 24, "group_count": 8, "problems": [], "groups": growth + bounded}
        report = MODULE.build_report(summary, support)
        self.assertEqual(report["problems"], [])
        self.assertEqual(report["family_count"], 2)
        self.assertEqual({f["state_evolution"] for f in report["families"]}, {"growth", "bounded"})
        self.assertEqual({f["bounded_churn_slots"] for f in report["families"]}, {0, 8192})

    def test_state_evolution_is_part_of_family_identity(self) -> None:
        growth = [group(c, r, state_evolution="growth", format_version=6) for c, r in ((1,100),(2,180),(4,300),(8,400))]
        bounded = [group(c, r, state_evolution="bounded", bounded_churn_slots=8192, format_version=6) for c, r in ((1,90),(2,170),(4,290),(8,390))]
        support = dict(self.support); support["case_count"] = 24
        summary = {"row_count": 24, "group_count": 8, "problems": [], "groups": growth + bounded}
        report = MODULE.build_report(summary, support)
        self.assertEqual(report["problems"], [])
        self.assertEqual(report["family_count"], 2)
        self.assertEqual({f["state_evolution"] for f in report["families"]}, {"growth", "bounded"})
        self.assertEqual({f["bounded_churn_slots"] for f in report["families"]}, {0, 8192})

    def test_read_materialization_is_family_identity(self) -> None:
        corrected = group(1, 100)
        legacy = group(1, 100)
        corrected["read_materialization"] = "full-record-v1"
        legacy["read_materialization"] = "legacy-read-v0"
        self.assertNotEqual(MODULE.identity(corrected), MODULE.identity(legacy))

    def test_write_materialization_is_family_identity(self) -> None:
        corrected = group(1, 100)
        legacy = group(1, 100)
        corrected["write_materialization"] = "no-return-v1"
        legacy["write_materialization"] = "legacy-return-v0"
        self.assertNotEqual(MODULE.identity(corrected), MODULE.identity(legacy))

    def test_incomplete_family_is_visible_during_progress(self) -> None:
        summary = {"row_count": 6, "group_count": 2, "problems": ["partial"], "groups": [group(1, 100), group(4, 250)]}
        report = MODULE.build_report(summary, self.support, allow_incomplete=True)
        self.assertEqual(report["complete_family_count"], 0)
        self.assertEqual(report["families"][0]["missing_clients"], [2, 8])
        self.assertIn("partial", report["problems"])

    def test_strict_mode_reports_global_and_family_incompleteness(self) -> None:
        summary = {"row_count": 6, "group_count": 2, "problems": [], "groups": [group(1, 100), group(4, 250)]}
        report = MODULE.build_report(summary, self.support)
        self.assertTrue(any("row_count" in p for p in report["problems"]))
        self.assertTrue(any("clients=" in p for p in report["problems"]))


if __name__ == "__main__":
    unittest.main()
