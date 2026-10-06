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


def audit_group(engine: str, workload: str, ops: int, status: str, suggested: int | None = None) -> dict[str, object]:
    return {
        "engine": engine,
        "engine_version": "1.0",
        "durability": "relaxed",
        "workload": workload,
        "records": 100,
        "ops_requested": ops,
        "status": status,
        "suggested_effective_ops": suggested,
    }


class SelectBaselineSizingResultsTests(unittest.TestCase):
    def test_run_id_accepts_record_prefix(self) -> None:
        target = audit_group("sqlite", "tiny-txn", 5_000, "undersized", 30_000)
        self.assertEqual(
            run_id(target, "20261006-record-resize"),
            "20261006-record-resize-sqlite-relaxed-tiny-txn-e30000",
        )

    def test_replaces_only_undersized_group(self) -> None:
        stock = [row("a", "point-read", t, 100) for t in (1, 2, 3)] + [row("b", "tiny-txn", t, 100) for t in (1, 2, 3)]
        target = audit_group("b", "tiny-txn", 100, "undersized", 300)
        audit = {"group_count": 2, "groups": [audit_group("a", "point-read", 100, "accepted"), target]}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run = root / run_id(target, "20261006-kv-resize")
            run.mkdir()
            resized = [row("b", "tiny-txn", t, 300) for t in (1, 2, 3)]
            (run / "results.ndjson").write_text("".join(json.dumps(item) + "\n" for item in resized))
            (run / "summary.json").write_text(json.dumps({"row_count": 3, "group_count": 1, "problems": []}))
            selected, manifest = select_rows(stock, audit, root, allow_missing_resize=False)
        self.assertEqual(len(selected), 6)
        self.assertEqual({int(item["ops_requested"]) for item in selected if item["engine"] == "a"}, {100})
        self.assertEqual({int(item["ops_requested"]) for item in selected if item["engine"] == "b"}, {300})
        self.assertEqual(manifest["selected_stock_groups"], 1)
        self.assertEqual(manifest["selected_resize_groups"], 1)
        self.assertTrue(manifest["complete"])

    def test_partial_mode_omits_pending_undersized_group(self) -> None:
        stock = [row("a", "point-read", t, 100) for t in (1, 2, 3)] + [row("b", "tiny-txn", t, 100) for t in (1, 2, 3)]
        target = audit_group("b", "tiny-txn", 100, "undersized", 300)
        audit = {"group_count": 2, "groups": [audit_group("a", "point-read", 100, "accepted"), target]}
        with tempfile.TemporaryDirectory() as tmp:
            selected, manifest = select_rows(stock, audit, Path(tmp), allow_missing_resize=True)
        self.assertEqual(len(selected), 3)
        self.assertEqual(manifest["pending_resize_groups"], 1)
        self.assertFalse(manifest["complete"])

    def test_rejects_resize_identity_mismatch(self) -> None:
        stock = [row("b", "tiny-txn", t, 100) for t in (1, 2, 3)]
        target = audit_group("b", "tiny-txn", 100, "undersized", 300)
        audit = {"group_count": 1, "groups": [target]}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run = root / run_id(target, "20261006-kv-resize")
            run.mkdir()
            resized = [row("b", "tiny-txn", t, 300) for t in (1, 2, 3)]
            resized[0]["configuration"] = "different"
            (run / "results.ndjson").write_text("".join(json.dumps(item) + "\n" for item in resized))
            (run / "summary.json").write_text(json.dumps({"row_count": 3, "group_count": 1, "problems": []}))
            with self.assertRaisesRegex(ValueError, "identity"):
                select_rows(stock, audit, root, allow_missing_resize=False)


if __name__ == "__main__":
    unittest.main()
