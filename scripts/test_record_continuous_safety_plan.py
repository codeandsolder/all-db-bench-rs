from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("make-record-continuous-safety-plan.py")
SPEC = importlib.util.spec_from_file_location("make_record_continuous_safety_plan", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


def group(engine: str, *, workload: str = "indexed-read", ops: int = 1000) -> dict:
    return {
        "engine": engine,
        "durability": "relaxed",
        "workload": workload,
        "records": 10000,
        "suggested_effective_ops": ops,
        "runner_ops_override": ops,
        "suggested_trials": 5,
        "quality_repair_required": True,
        "resize_strategy": "quality-repair",
    }


def write_run(root: Path, prefix: str, g: dict, rate: float) -> None:
    rid = M.run_id(g, prefix)
    run = root / rid
    run.mkdir(parents=True)
    rows = []
    for trial in range(1, 6):
        rows.append(
            {
                "engine": g["engine"],
                "durability": g["durability"],
                "workload": g["workload"],
                "ops_requested": g["suggested_effective_ops"],
                "ops_per_s": rate,
                "elapsed_s": g["suggested_effective_ops"] / rate,
                "trial": trial,
            }
        )
    (run / "results.ndjson").write_text("".join(json.dumps(row) + "\n" for row in rows))
    (run / "summary.json").write_text(json.dumps({"row_count": 5, "group_count": 1, "problems": []}))


class ContinuousSafetyPlanTests(unittest.TestCase):
    def parent(self) -> dict:
        engines = ["sqlite", "turso", "surrealdb", "surrealdb-rocksdb"]
        near = [group(engine) for engine in engines]
        safe = [group(engine, workload="read-heavy", ops=2000) for engine in engines]
        return {
            "quality_policy_version": 1,
            "strategy": "final-five-trial-common-work-v6",
            "groups": near + safe,
            "common_effective_ops": {
                "relaxed/indexed-read": 1000,
                "relaxed/read-heavy": 2000,
            },
            "estimated_duration_bounds": {
                "relaxed/indexed-read": {"estimated_fastest_s": 2.0},
                "relaxed/read-heavy": {"estimated_fastest_s": 3.0},
            },
        }

    def test_only_near_floor_family_is_scaled(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            parent = self.parent()
            rates = {
                "sqlite": 500.0,
                "turso": 400.0,
                "surrealdb": 300.0,
                "surrealdb-rocksdb": 250.0,
            }
            for g in parent["groups"][:4]:
                write_run(root, "old", g, rates[g["engine"]])
            out = M.make_safety_plan(
                parent,
                parent_sha256="a" * 64,
                run_root=root,
                run_prefix="old",
                target_fastest_seconds=2.2,
            )
            self.assertEqual(out["common_effective_ops"]["relaxed/indexed-read"], 1100)
            self.assertEqual(out["common_effective_ops"]["relaxed/read-heavy"], 2000)
            self.assertEqual(set(out["continuous_safety_families"]), {"relaxed/indexed-read"})
            indexed = [g for g in out["groups"] if g["workload"] == "indexed-read"]
            read_heavy = [g for g in out["groups"] if g["workload"] == "read-heavy"]
            self.assertEqual({g["suggested_effective_ops"] for g in indexed}, {1100})
            self.assertEqual({g["suggested_effective_ops"] for g in read_heavy}, {2000})

    def test_safe_family_does_not_require_historical_results(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            parent = self.parent()
            for g in parent["groups"][:4]:
                write_run(root, "old", g, 500.0)
            out = M.make_safety_plan(
                parent,
                parent_sha256="b" * 64,
                run_root=root,
                run_prefix="old",
                target_fastest_seconds=2.2,
            )
            self.assertNotIn("relaxed/read-heavy", out["continuous_safety_families"])

    def test_missing_candidate_evidence_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            parent = self.parent()
            for g in parent["groups"][:3]:
                write_run(root, "old", g, 500.0)
            with self.assertRaisesRegex(ValueError, "missing historical confirmation result"):
                M.make_safety_plan(
                    parent,
                    parent_sha256="c" * 64,
                    run_root=root,
                    run_prefix="old",
                    target_fastest_seconds=2.2,
                )


if __name__ == "__main__":
    unittest.main()
