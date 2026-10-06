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


def row(engine: str, workload: str, trial: int, ops: int) -> dict[str, object]:
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
        "elapsed_s": 1.0,
        "ops_per_s": float(ops),
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
        "thresholds": {"expect_trials": 3},
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
            selected, manifest = select_rows(stock, plan, root, allow_missing_resize=False)
        self.assertEqual(len(selected), 8)
        self.assertEqual([item["trial"] for item in selected if item["engine"] == "b"], [1, 2, 3, 4, 5])
        self.assertEqual({int(item["ops_requested"]) for item in selected if item["engine"] == "b"}, {100})
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

    def test_rejects_old_policy(self) -> None:
        with self.assertRaisesRegex(ValueError, "unsupported sizing policy"):
            select_rows([], {"sizing_policy_version": 1, "groups": []}, Path("/tmp"), allow_missing_resize=True)


if __name__ == "__main__":
    unittest.main()
