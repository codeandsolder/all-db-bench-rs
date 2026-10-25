from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
QUICK = (ROOT / "scripts/run-record-quick-idle.py").read_text()
FOLLOWUP = (ROOT / "scripts/run-record-sizing-followups.py").read_text()


class RecordRunnerDefaultTests(unittest.TestCase):
    def test_corrected_record_runners_require_explicit_pinned_binaries(self) -> None:
        for source in (QUICK, FOLLOWUP):
            self.assertIn('parser.add_argument("--bench-bin", type=Path, required=True)', source)
            self.assertIn('parser.add_argument("--rocks-bench-bin", type=Path, required=True)', source)
            self.assertNotIn("recordbench-ae74103b847171d1", source)
            self.assertNotIn("surrealdb-rocksdb-recordbench-c3978a3b66a24edd", source)

    def test_followup_plan_is_explicit_and_new_prefix_is_default(self) -> None:
        self.assertIn('parser.add_argument("--plan", type=Path, required=True)', FOLLOWUP)
        self.assertIn('20261009-record-full-v1-resize-v1', FOLLOWUP)

    def test_finalizer_uses_lane_specific_explicit_inputs(self) -> None:
        finalizer = (ROOT / "scripts/finalize-baseline-sizing.py").read_text()
        self.assertIn('parser.add_argument("--record-stock-results", type=Path)', finalizer)
        self.assertIn('parser.add_argument("--record-audit", type=Path)', finalizer)
        self.assertIn('record_stock_results = required_path(parser, args.record_stock_results, "--record-stock-results")', finalizer)
        self.assertIn('record_audit = required_path(parser, args.record_audit, "--record-audit")', finalizer)
        self.assertNotIn("20261006-record-quick-stock-v4.json", finalizer)
        self.assertNotIn("20261006-record-stock-pressure-repairs-refined-v1.json", finalizer)
        self.assertNotIn("20261009-record-full-v1-resize-v1", finalizer)


if __name__ == "__main__":
    unittest.main()
