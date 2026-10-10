from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check-kv-baseline-selection-ready.py")
SPEC = importlib.util.spec_from_file_location("check_kv_baseline_selection_ready", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


class CheckKvBaselineSelectionReadyTests(unittest.TestCase):
    def fixture(self, root: Path) -> Path:
        selected = root / "selected"
        selected.mkdir()
        rows = [{"x": 1}, {"x": 2}]
        results = selected / "results.ndjson"
        results.write_text("".join(json.dumps(row) + "\n" for row in rows))
        manifest = {
            "selection_version": 5, "admission_repair_complete": True, "selected_groups": 2,
            "repaired_groups": [{"x": 1}], "final_status_counts": {"accepted": 2},
            "selected_followup_rejected_pressure_attempts": 0, "repair_plan_sha256": "plan",
            "repair_initial_min_free_gib": 10, "repair_case_min_free_gib": "10",
        }
        manifest_path = selected / "manifest.json"
        manifest_path.write_text(json.dumps(manifest))
        complete = {
            "completion_version": 1, "selection_version": 5, "admission_repair_complete": True,
            "selected_rows": 2, "selected_groups": 2, "repaired_groups": 1,
            "results_sha256": M.sha256(results), "manifest_sha256": M.sha256(manifest_path),
            "repair_plan_sha256": "plan",
        }
        (selected / "complete.json").write_text(json.dumps(complete))
        return selected

    def test_accepts_matching_completion(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            selected = self.fixture(Path(tmp))
            report = M.validate(selected, expected_rows=2, expected_groups=2, expected_repairs=1)
            self.assertEqual(report["selected_rows"], 2)

    def test_rejects_payload_changed_after_completion(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            selected = self.fixture(Path(tmp))
            (selected / "results.ndjson").write_text('{"x":3}\n{"x":4}\n')
            with self.assertRaisesRegex(ValueError, "results hash mismatch"):
                M.validate(selected, expected_rows=2, expected_groups=2, expected_repairs=1)

    def test_rejects_missing_completion_marker(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            selected = self.fixture(Path(tmp))
            (selected / "complete.json").unlink()
            with self.assertRaisesRegex(ValueError, "missing v5 selection artifact"):
                M.validate(selected, expected_rows=2, expected_groups=2, expected_repairs=1)


if __name__ == "__main__":
    unittest.main()
