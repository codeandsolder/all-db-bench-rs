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
    def test_unit_requires_continuously_guarded_record_baseline_and_hands_off_to_kv(self) -> None:
        runtime = Path("/srv/runtime")
        text = M.unit_text(runtime)
        self.assertIn(
            "After=local-fs.target all-db-bench-record-final-continuous.service",
            text,
        )
        self.assertIn(
            "ConditionPathExists=/srv/scratch/db-bench-work/record-full-v1/ready-v4.json",
            text,
        )
        self.assertIn(
            "ConditionPathExists=/srv/scratch/db-bench-work/record-full-v1/selected-final-v4/manifest.json",
            text,
        )
        self.assertIn(
            "ConditionPathExists=!/srv/scratch/db-bench-work/kv-sizing-audit/selected-final-v5/complete.json",
            text,
        )
        self.assertIn("verify-record-ready.py", text)
        self.assertIn("--selected /srv/scratch/db-bench-work/record-full-v1/selected-final-v4", text)
        self.assertIn("--repo /srv/scratch/db-bench-work/record-final-continuous-runtime", text)
        self.assertIn(
            "--expected-admission-policy pre-io+pre/continuous/post-external-v3",
            text,
        )
        self.assertIn(
            "--expected-admission-policy pre-io+pre/post-external-v2 --expected-bench-sha256",
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

    def test_record_handoff_targets_continuous_confirmation(self) -> None:
        self.assertEqual(
            M.record_handoff_dropin(),
            "[Unit]\nConditionPathExists=!/srv/scratch/db-bench-work/record-full-v1/ready.json\nOnSuccess=all-db-bench-record-final-continuous.service\n",
        )

    def test_kv_gate_requires_verified_v5_completion(self) -> None:
        text = M.kv_v5_gate_dropin(Path("/srv/runtime"))
        self.assertIn(
            "ConditionPathExists=/srv/scratch/db-bench-work/kv-sizing-audit/selected-final-v5/complete.json",
            text,
        )
        self.assertIn("/srv/runtime/scripts/check-kv-baseline-selection-ready.py", text)
        self.assertIn("--expected-rows 1046 --expected-groups 180 --expected-repairs 5", text)


if __name__ == "__main__":
    unittest.main()
