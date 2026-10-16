from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).with_name("finalize-baseline-sizing.py")
SPEC = importlib.util.spec_from_file_location("finalize_baseline_sizing", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class FinalizeBaselineSizingTests(unittest.TestCase):
    def invoke(self, *args: str) -> int:
        with mock.patch.object(sys, "argv", [str(SCRIPT), *args]):
            return MODULE.main()

    def test_kv_only_forwards_generation_paths(self) -> None:
        with mock.patch.object(MODULE, "finalize_one") as finalize:
            rc = self.invoke(
                "--repo", "/repo", "--kv-only",
                "--kv-stock-results", "/stock.ndjson",
                "--kv-audit", "/audit.json",
                "--kv-resize-prefix", "20261016-refresh-kv-resize",
                "--kv-out-dir", "/selected-kv",
                "--kv-quality-repair-plan", "/quality.json",
            )
        self.assertEqual(rc, 0)
        finalize.assert_called_once_with(
            Path("/repo"),
            stock_results=Path("/stock.ndjson"),
            audit=Path("/audit.json"),
            resize_prefix="20261016-refresh-kv-resize",
            out_dir=Path("/selected-kv"),
            expected_groups=180,
            quality_repair_plan=Path("/quality.json"),
        )

    def test_record_only_does_not_require_kv_paths(self) -> None:
        with mock.patch.object(MODULE, "finalize_one") as finalize:
            rc = self.invoke(
                "--repo", "/repo", "--record-only",
                "--record-stock-results", "/record.ndjson",
                "--record-audit", "/record-audit.json",
                "--record-out-dir", "/selected-record",
                "--record-skip-quality-repairs",
            )
        self.assertEqual(rc, 0)
        self.assertEqual(finalize.call_count, 1)
        self.assertEqual(finalize.call_args.kwargs["expected_groups"], 40)
        self.assertIsNone(finalize.call_args.kwargs["quality_repair_plan"])

    def test_selected_lane_requires_its_paths(self) -> None:
        with self.assertRaises(SystemExit) as error:
            self.invoke("--repo", "/repo", "--kv-only")
        self.assertEqual(error.exception.code, 2)

    def test_default_runs_both_lanes(self) -> None:
        with mock.patch.object(MODULE, "finalize_one") as finalize:
            rc = self.invoke(
                "--repo", "/repo",
                "--kv-stock-results", "/kv.ndjson",
                "--kv-audit", "/kv-audit.json",
                "--kv-out-dir", "/kv-out",
                "--kv-skip-quality-repairs",
                "--record-stock-results", "/record.ndjson",
                "--record-audit", "/record-audit.json",
                "--record-out-dir", "/record-out",
                "--record-skip-quality-repairs",
            )
        self.assertEqual(rc, 0)
        self.assertEqual(finalize.call_count, 2)
        self.assertEqual([call.kwargs["expected_groups"] for call in finalize.call_args_list], [180, 40])


if __name__ == "__main__":
    unittest.main()
