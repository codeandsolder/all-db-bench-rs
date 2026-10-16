from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("select-baseline-sizing-results.py")
SPEC = importlib.util.spec_from_file_location("select_baseline_sizing_results", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)
select_rows = MODULE.select_rows
run_id = MODULE.run_id


def row(engine: str, workload: str, trial: int, ops: int, *, elapsed: float = 1.0, rate: float | None = None) -> dict[str, object]:
    return {
        "lane": "kv",
        "scenario": "baseline-core",
        "engine": engine,
        "engine_version": "1.0",
        "durability": "relaxed",
        "workload": workload,
        "records": 100,
        "ops_requested": ops,
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
        "ops_per_s": float(ops if rate is None else rate),
    }


def audit_group(
    engine: str,
    workload: str,
    ops: int,
    status: str,
    suggested: int | None = None,
    trials: int | None = None,
    strategy: str | None = None,
) -> dict[str, object]:
    return {
        "engine": engine,
        "engine_version": "1.0",
        "durability": "relaxed",
        "workload": workload,
        "records": 100,
        "ops_requested": ops,
        "status": status,
        "resize_strategy": strategy,
        "suggested_effective_ops": suggested,
        "suggested_trials": trials,
    }


def audit(*groups: dict[str, object]) -> dict[str, object]:
    return {
        "sizing_policy_version": 2,
        "group_count": len(groups),
        "thresholds": {
            "expect_trials": 3,
            "max_cv": 0.10,
            "max_relative_spread": 0.25,
            "read_only_min_seconds": 0.5,
            "read_only_workloads": ["indexed-read", "point-read", "range-scan"],
            "stateful_min_total_seconds": 1.0,
        },
        "groups": list(groups),
    }


class SelectBaselineSizingResultsTests(unittest.TestCase):
    def test_run_id_accepts_record_prefix_and_trial_count(self) -> None:
        target = audit_group("sqlite", "tiny-txn", 5_000, "undersized", 5_000, 11, "more-trials")
        self.assertEqual(
            run_id(target, "20261006-record-resize-v4"),
            "20261006-record-resize-v4-sqlite-relaxed-tiny-txn-e5000-t11",
        )

    def test_replaces_stateful_group_with_more_trials_same_ops(self) -> None:
        stock = [row("a", "point-read", t, 100) for t in (1, 2, 3)] + [
            row("b", "tiny-txn", t, 100) for t in (1, 2, 3)
        ]
        target = audit_group("b", "tiny-txn", 100, "undersized", 100, 5, "more-trials")
        plan = audit(audit_group("a", "point-read", 100, "accepted"), target)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run = root / run_id(target, "20261006-kv-resize-v4")
            run.mkdir()
            resized = [row("b", "tiny-txn", t, 100) for t in range(1, 6)]
            (run / "results.ndjson").write_text("".join(json.dumps(item) + "\n" for item in resized))
            (run / "summary.json").write_text(json.dumps({"row_count": 5, "group_count": 1, "problems": []}))
            rejected = run / "rejected-pressure" / "t4-b-relaxed-tiny-txn" / "attempt-001"
            rejected.mkdir(parents=True)
            (rejected / "rejection.json").write_text("{}")
            selected, manifest = select_rows(stock, plan, root, allow_missing_resize=False)
        self.assertEqual(len(selected), 8)
        self.assertEqual([item["trial"] for item in selected if item["engine"] == "b"], [1, 2, 3, 4, 5])
        self.assertEqual({int(item["ops_requested"]) for item in selected if item["engine"] == "b"}, {100})
        source = next(item for item in manifest["sources"] if item["engine"] == "b")
        self.assertEqual(source["rejected_pressure_attempts"], 1)
        self.assertEqual(manifest["selected_followup_rejected_pressure_attempts"], 1)
        self.assertTrue(manifest["complete"])

    def test_replaces_read_only_group_with_more_ops(self) -> None:
        stock = [row("a", "point-read", t, 100) for t in (1, 2, 3)]
        target = audit_group("a", "point-read", 100, "undersized", 300, 3, "more-ops")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run = root / run_id(target, "20261006-kv-resize-v4")
            run.mkdir()
            resized = [row("a", "point-read", t, 300) for t in (1, 2, 3)]
            (run / "results.ndjson").write_text("".join(json.dumps(item) + "\n" for item in resized))
            (run / "summary.json").write_text(json.dumps({"row_count": 3, "group_count": 1, "problems": []}))
            selected, _ = select_rows(stock, audit(target), root, allow_missing_resize=False)
        self.assertEqual({int(item["ops_requested"]) for item in selected}, {300})

    def test_partial_mode_omits_pending_undersized_group(self) -> None:
        stock = [row("a", "point-read", t, 100) for t in (1, 2, 3)] + [
            row("b", "tiny-txn", t, 100) for t in (1, 2, 3)
        ]
        target = audit_group("b", "tiny-txn", 100, "undersized", 100, 5, "more-trials")
        plan = audit(audit_group("a", "point-read", 100, "accepted"), target)
        with tempfile.TemporaryDirectory() as tmp:
            selected, manifest = select_rows(stock, plan, Path(tmp), allow_missing_resize=True)
        self.assertEqual(len(selected), 3)
        self.assertEqual(manifest["pending_resize_groups"], 1)
        self.assertFalse(manifest["complete"])

    def test_rejects_resize_identity_mismatch(self) -> None:
        stock = [row("b", "tiny-txn", t, 100) for t in (1, 2, 3)]
        target = audit_group("b", "tiny-txn", 100, "undersized", 100, 5, "more-trials")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run = root / run_id(target, "20261006-kv-resize-v4")
            run.mkdir()
            resized = [row("b", "tiny-txn", t, 100) for t in range(1, 6)]
            resized[0]["configuration"] = "different"
            (run / "results.ndjson").write_text("".join(json.dumps(item) + "\n" for item in resized))
            (run / "summary.json").write_text(json.dumps({"row_count": 5, "group_count": 1, "problems": []}))
            with self.assertRaisesRegex(ValueError, "identity"):
                select_rows(stock, audit(target), root, allow_missing_resize=False)


    def test_quality_repair_replaces_retained_variable_stock(self) -> None:
        stock = [row("q", "write-burst", t, 100, elapsed=0.3, rate=rate) for t, rate in enumerate((100.0, 70.0, 100.0), 1)]
        variable = audit_group("q", "write-burst", 100, "variable")
        plan = audit(variable)
        repair = {
            **variable,
            "quality_repair_required": True,
            "resize_strategy": "quality-repair",
            "suggested_effective_ops": 100,
            "runner_ops_override": 100,
            "suggested_trials": 3,
        }
        quality_plan = {"quality_policy_version": 1, "groups": [repair]}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run = root / run_id(repair, "20261006-kv-resize-v4")
            run.mkdir()
            repaired = [row("q", "write-burst", t, 100, elapsed=0.35, rate=100.0 + t) for t in (1, 2, 3)]
            (run / "results.ndjson").write_text("".join(json.dumps(item) + "\n" for item in repaired))
            (run / "summary.json").write_text(json.dumps({"row_count": 3, "group_count": 1, "problems": []}))
            selected, manifest = select_rows(
                stock, plan, root, allow_missing_resize=False, quality_repair_plan=quality_plan
            )
        self.assertEqual(len(selected), 3)
        self.assertEqual(manifest["selected_stock_groups"], 0)
        self.assertEqual(manifest["selected_quality_repair_groups"], 1)
        self.assertEqual(manifest["sources"][0]["source"], "quality-repair")
        self.assertEqual(manifest["sources"][0]["final_status"], "accepted")

    def test_final_confirmation_overrides_undersized_group_with_different_ops(self) -> None:
        stock = [row("sqlite", "point-read", t, 100, elapsed=0.2) for t in (1, 2, 3)]
        target = audit_group("sqlite", "point-read", 100, "undersized", 300, 3, "more-ops")
        plan = audit(target)
        confirmation = {
            **target,
            "suggested_effective_ops": 200,
            "suggested_trials": 5,
            "resize_strategy": "quality-repair",
            "quality_repair_required": True,
        }
        quality_plan = {
            "quality_policy_version": 1,
            "strategy": "final-five-trial-common-work-v3",
            "source_selection": "selected-final-v2-sizing-only",
            "groups": [confirmation],
        }
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run = root / run_id(confirmation, "20261010-record-final-v3")
            run.mkdir()
            repaired = [
                row("sqlite", "point-read", t, 200, elapsed=1.0, rate=200.0 + t)
                for t in range(1, 6)
            ]
            (run / "results.ndjson").write_text(
                "".join(json.dumps(item) + "\n" for item in repaired)
            )
            (run / "summary.json").write_text(
                json.dumps({"row_count": 5, "group_count": 1, "problems": []})
            )
            selected, manifest = select_rows(
                stock,
                plan,
                root,
                resize_prefix="20261010-record-final-v3",
                allow_missing_resize=False,
                quality_repair_plan=quality_plan,
            )
        self.assertEqual(len(selected), 5)
        self.assertEqual({int(item["ops_requested"]) for item in selected}, {200})
        self.assertEqual(manifest["selected_stock_groups"], 0)
        self.assertEqual(manifest["selected_resize_groups"], 0)
        self.assertEqual(manifest["selected_quality_repair_groups"], 1)

    def test_policy_v3_requires_explicit_record_semantics(self) -> None:
        stock = [dict(row("sqlite", "point-read", trial, 100), lane="record") for trial in (1, 2, 3)]
        group = audit_group("sqlite", "point-read", 100, "accepted")
        group["read_materialization"] = "full-record-v1"
        group["write_materialization"] = "no-return-v1"
        plan = audit(group)
        plan["sizing_policy_version"] = 3
        with self.assertRaisesRegex(ValueError, "require explicit read/write materialization"):
            select_rows(stock, plan, Path("/tmp"), allow_missing_resize=True)

    def test_policy_v3_read_semantics_are_part_of_selection_identity(self) -> None:
        stock = [
            dict(
                row("sqlite", "point-read", trial, 100),
                lane="record",
                read_materialization="full-record-v1",
                write_materialization="no-return-v1",
            )
            for trial in (1, 2, 3)
        ]
        group = audit_group("sqlite", "point-read", 100, "accepted")
        group["read_materialization"] = "full-record-v1"
        group["write_materialization"] = "no-return-v1"
        plan = audit(group)
        plan["sizing_policy_version"] = 3
        selected, manifest = select_rows(stock, plan, Path("/tmp"), allow_missing_resize=False)
        self.assertEqual(len(selected), 3)
        self.assertTrue(manifest["complete"])

        wrong = dict(group, read_materialization="legacy-read-v0")
        wrong_plan = audit(wrong)
        wrong_plan["sizing_policy_version"] = 3
        with self.assertRaisesRegex(ValueError, "missing from audit"):
            select_rows(stock, wrong_plan, Path("/tmp"), allow_missing_resize=False)

    def test_rejects_old_policy(self) -> None:
        with self.assertRaisesRegex(ValueError, "unsupported sizing policy"):
            select_rows([], {"sizing_policy_version": 1, "groups": []}, Path("/tmp"), allow_missing_resize=True)


if __name__ == "__main__":
    unittest.main()
