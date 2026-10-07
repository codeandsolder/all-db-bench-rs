from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("refine-stock-quality-repairs.py")
SPEC = importlib.util.spec_from_file_location("refine_stock_quality_repairs", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def row(*, engine: str, workload: str, trial: int, elapsed: float, ops: int = 50_000) -> dict[str, object]:
    return {
        "lane": "kv",
        "scenario": "baseline-core",
        "engine": engine,
        "engine_version": "1",
        "durability": "relaxed",
        "workload": workload,
        "records": 100_000,
        "ops_requested": ops,
        "trial": trial,
        "elapsed_s": elapsed,
        "ops_per_s": ops / elapsed,
    }


def group(engine: str, workload: str) -> dict[str, object]:
    return {
        "engine": engine,
        "engine_version": "1",
        "durability": "relaxed",
        "workload": workload,
        "records": 100_000,
        "ops_requested": 50_000,
        "status": "variable",
        "resize_strategy": "quality-repair",
        "suggested_effective_ops": 50_000,
        "runner_ops_override": 50_000,
        "suggested_trials": 3,
        "median_elapsed_s": 0.3,
    }


def sizing() -> dict[str, object]:
    return {
        "sizing_policy_version": 2,
        "thresholds": {
            "read_only_min_seconds": 2.0,
            "read_only_target_seconds": 3.0,
            "stateful_min_total_seconds": 0.75,
            "stateful_target_total_seconds": 1.0,
            "stateful_max_trials": 51,
            "max_cv": 0.10,
            "max_relative_spread": 0.25,
        },
    }


def write_run(root: Path, item: dict[str, object], rows: list[dict[str, object]]) -> None:
    rid = MODULE.run_id(item, "resize")
    run = root / rid
    run.mkdir(parents=True)
    (run / "results.ndjson").write_text("".join(json.dumps(row) + "\n" for row in rows))
    (run / "summary.json").write_text(json.dumps({"row_count": len(rows), "group_count": 1, "problems": []}))


class RefineStockQualityRepairsTests(unittest.TestCase):
    def test_promotes_clean_stateful_repair_to_more_trials(self) -> None:
        item = group("heed", "read-heavy")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_run(root, item, [row(engine="heed", workload="read-heavy", trial=t, elapsed=0.21) for t in (1, 2, 3)])
            refined = MODULE.refine_plan({"quality_policy_version": 1, "groups": [item]}, sizing(), root, resize_prefix="resize")
        result = refined["groups"][0]
        self.assertTrue(result["quality_repair_refined"])
        self.assertEqual(result["quality_repair_refinement_strategy"], "more-trials")
        self.assertEqual(result["suggested_effective_ops"], 50_000)
        self.assertEqual(result["suggested_trials"], 5)
        self.assertEqual(refined["refined_group_count"], 1)

    def test_keeps_clean_sufficient_repair_at_three_trials(self) -> None:
        item = group("mdbx", "write-burst")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_run(root, item, [row(engine="mdbx", workload="write-burst", trial=t, elapsed=0.30) for t in (1, 2, 3)])
            refined = MODULE.refine_plan({"quality_policy_version": 1, "groups": [item]}, sizing(), root, resize_prefix="resize")
        result = refined["groups"][0]
        self.assertFalse(result["quality_repair_refined"])
        self.assertEqual(result["suggested_trials"], 3)
        self.assertEqual(refined["refined_group_count"], 0)

    def test_promotes_read_only_repair_by_ops_not_trials(self) -> None:
        item = group("redb", "point-read")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_run(root, item, [row(engine="redb", workload="point-read", trial=t, elapsed=0.5) for t in (1, 2, 3)])
            refined = MODULE.refine_plan({"quality_policy_version": 1, "groups": [item]}, sizing(), root, resize_prefix="resize")
        result = refined["groups"][0]
        self.assertEqual(result["quality_repair_refinement_strategy"], "more-ops")
        self.assertEqual(result["suggested_trials"], 3)
        self.assertGreater(result["suggested_effective_ops"], 50_000)


if __name__ == "__main__":
    unittest.main()
