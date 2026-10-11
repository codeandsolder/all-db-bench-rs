from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("make-record-final-continuous-unit.py")
SPEC = importlib.util.spec_from_file_location("make_record_final_continuous_unit", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


class RecordFinalContinuousUnitTests(unittest.TestCase):
    def manifest(self) -> dict:
        return {
            "repo_commit": M.BINARY_COMMIT,
            "read_materialization": "full-record-v1",
            "write_materialization": "no-return-v1",
            "binaries": {
                "recordbench": {"path": "/bin/recordbench", "sha256": "a" * 64},
                "surrealdb-rocksdb-recordbench": {
                    "path": "/bin/rocks-recordbench",
                    "sha256": "b" * 64,
                },
            },
        }

    def test_unit_runs_one_homogeneous_continuous_confirmation(self) -> None:
        text = M.unit_text(self.manifest(), Path("/x/runtime"))
        self.assertIn("After=local-fs.target", text)
        self.assertNotIn("After=local-fs.target all-db-bench-record-final-v3.service", text)
        self.assertNotIn(
            "ConditionPathExists=/srv/scratch/db-bench-work/record-full-v1/ready.json", text
        )
        self.assertNotIn("selected-final-v3/manifest.json", text)
        self.assertIn(
            "ConditionPathExists=/srv/scratch/db-bench-work/record-full-v1/runtime/results/runs/"
            "20261010-record-full-v1-stock-v2/results.ndjson",
            text,
        )
        self.assertIn(
            "ConditionPathExists=/srv/scratch/db-bench-work/record-full-v1/"
            "20261010-record-full-v1-stock-v2-sizing-v3.json",
            text,
        )
        self.assertIn("ConditionPathExists=!/srv/scratch/db-bench-work/record-full-v1/ready-v4.json", text)
        self.assertEqual(text.count("run-record-sizing-followups.py"), 1)
        self.assertEqual(text.count("make-record-continuous-safety-plan.py"), 1)
        self.assertIn("--target-fastest-seconds 2.2", text)
        self.assertIn("--run-prefix 20261010-record-final-v3", text)
        self.assertIn("--out /srv/scratch/db-bench-work/record-full-v1/20261010-record-final-v4-continuous-plan.json", text)
        self.assertIn("--quality-plan /srv/scratch/db-bench-work/record-full-v1/20261010-record-final-v4-continuous-plan.json", text)
        self.assertIn("--record-quality-repair-plan /srv/scratch/db-bench-work/record-full-v1/20261010-record-final-v4-continuous-plan.json", text)
        self.assertIn("--run-prefix 20261010-record-final-v4-continuous", text)
        self.assertIn("--record-out-dir /srv/scratch/db-bench-work/record-full-v1/selected-final-v4", text)
        self.assertIn("--expected-admission-policy pre-io+pre/continuous/post-external-v3", text)
        self.assertIn("--out /srv/scratch/db-bench-work/record-full-v1/ready-v4.json", text)
        self.assertIn("OnSuccess=all-db-bench-kv-final-continuous.service", text)

    def test_v2_handoff_targets_continuous_service(self) -> None:
        text = M.v2_handoff_dropin()
        self.assertIn("ConditionPathExists=!/srv/scratch/db-bench-work/record-full-v1/ready.json", text)
        self.assertIn("OnSuccess=all-db-bench-record-final-continuous.service", text)
        self.assertNotIn("kv-baseline-admission-repair", text)

    def test_record_downstream_dropin_requires_v5_continuous_certificate(self) -> None:
        text = M.record_ready_v5_dropin(Path("/record/runtime"), M.KV_DELETE_SCALING)
        self.assertIn("[Unit]\nConditionPathExists=\n", text)
        self.assertIn("ConditionPathExists=/srv/scratch/db-bench-work/concurrency-steady-runtime/results/runs/20261010-kv-concurrency-delete-diagnostic-v4/scaling.json", text)
        self.assertIn("ConditionPathExists=/srv/scratch/db-bench-work/record-full-v1/ready-v4.json", text)
        self.assertIn("ConditionPathExists=/srv/scratch/db-bench-work/kv-sizing-audit/ready-v6.json", text)
        self.assertIn("/record/runtime/scripts/check-record-ready-certificate.py", text)
        self.assertIn("--expected-admission-policy pre-io+pre/continuous/post-external-v3", text)

    def test_sustained_gate_preserves_full_prerequisite_set(self) -> None:
        text = M.sustained_ready_v5_dropin()
        self.assertTrue(text.startswith("[Unit]\nConditionPathExists=\n"))
        self.assertIn("ConditionPathExists=/srv/scratch/db-bench-work/record-concurrency-steady-runtime/results/runs/20261010-record-concurrency-steady-final-v3/scaling.json", text)
        self.assertIn("ConditionPathExists=/srv/scratch/db-bench-work/sustained-quick/bin/manifest.json", text)
        self.assertIn("ConditionPathExists=/srv/scratch/db-bench-work/record-full-v1/ready-v4.json", text)
        self.assertIn("ConditionPathExists=/srv/scratch/db-bench-work/kv-sizing-audit/ready-v6.json", text)

if __name__ == "__main__":
    unittest.main()
