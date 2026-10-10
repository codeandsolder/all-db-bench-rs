from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("make-kv-baseline-admission-repair-unit.py")
SPEC = importlib.util.spec_from_file_location("make_kv_baseline_admission_repair_unit", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


class KvBaselineAdmissionRepairUnitTests(unittest.TestCase):
    def test_unit_is_fail_closed_and_hands_off_to_kv(self) -> None:
        runtime = Path("/srv/runtime")
        text = M.unit_text(runtime)
        self.assertIn(
            "ConditionPathExists=/srv/scratch/db-bench-work/record-full-v1/ready.json",
            text,
        )
        self.assertIn(
            "ConditionPathExists=!/srv/scratch/db-bench-work/kv-sizing-audit/selected-final-v5/manifest.json",
            text,
        )
        self.assertIn("--quality-only", text)
        self.assertIn("--run-prefix 20261010-kv-admission-v2-repair", text)
        self.assertIn(
            "--expected-bench-sha256 360c3b398babd4710a179cf102ad2cfe1fbd5e39fcf32a288fc771a75fa7f563",
            text,
        )
        self.assertIn("finalize-kv-baseline-admission-repairs.py", text)
        self.assertIn("OnSuccess=all-db-bench-concurrency-sizing.service", text)

    def test_record_handoff_resets_old_target(self) -> None:
        text = M.record_handoff_dropin()
        self.assertEqual(
            text,
            "[Unit]\nOnSuccess=\nOnSuccess=all-db-bench-kv-baseline-admission-repair.service\n",
        )


if __name__ == "__main__":
    unittest.main()
