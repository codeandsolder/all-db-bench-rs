from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("verify-record-ready.py")
SPEC = importlib.util.spec_from_file_location("verify_record_ready", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


class VerifyRecordReadyTests(unittest.TestCase):
    def test_runner_commit_requires_clean_tracked_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.email", "test@example.invalid"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Test"], cwd=repo, check=True)
            tracked = repo / "tracked.txt"
            tracked.write_text("clean\n")
            subprocess.run(["git", "add", "tracked.txt"], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-qm", "init"], cwd=repo, check=True)
            expected = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            self.assertEqual(M.checked_runner_commit(repo), expected)
            (repo / "untracked.txt").write_text("allowed\n")
            self.assertEqual(M.checked_runner_commit(repo), expected)
            tracked.write_text("dirty\n")
            with self.assertRaisesRegex(SystemExit, "tracked modifications"):
                M.checked_runner_commit(repo)

    def test_no_stock_support_is_required_when_no_stock_group_is_selected(self) -> None:
        manifest = {
            "selected_stock_groups": 0,
            "selected_resize_groups": 0,
            "selected_quality_repair_groups": 2,
            "sources": [
                {"source": "quality-repair", "run_id": "fresh-a"},
                {"source": "quality-repair", "run_id": "fresh-b"},
            ],
        }
        self.assertEqual(M.selected_run_ids(manifest, "old-stock"), ["fresh-a", "fresh-b"])

    def test_continuous_support_provenance_must_be_homogeneous(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            for run_id in ("a", "b"):
                run = repo / "results" / "runs" / run_id
                run.mkdir(parents=True)
                (run / "support.json").write_text(
                    json.dumps(
                        {
                            "admission_policy": M.CONTINUOUS_ADMISSION_POLICY,
                            "noise_guard_sha256": "1" * 64,
                            "continuous_noise_guard_sha256": "2" * 64,
                            "continuous_noise_sample_ms": 250,
                            "continuous_noise_max_cpu_percent": 50,
                            "continuous_noise_max_io_average_mib_s": 2,
                            "continuous_noise_max_io_rate_mib_s": 8,
                        }
                    )
                )
            hashes, noise, continuous = M.load_supports(
                repo,
                ["a", "b"],
                expected_admission_policy=M.CONTINUOUS_ADMISSION_POLICY,
            )
            self.assertEqual(set(hashes), {"a", "b"})
            self.assertEqual(noise, "1" * 64)
            assert continuous is not None
            self.assertEqual(continuous["guard_sha256"], "2" * 64)
            self.assertEqual(continuous["sample_ms"], 250)

            support = json.loads((repo / "results" / "runs" / "b" / "support.json").read_text())
            support["continuous_noise_sample_ms"] = 500
            (repo / "results" / "runs" / "b" / "support.json").write_text(json.dumps(support))
            with self.assertRaisesRegex(SystemExit, "inconsistent continuous admission provenance"):
                M.load_supports(
                    repo,
                    ["a", "b"],
                    expected_admission_policy=M.CONTINUOUS_ADMISSION_POLICY,
                )


if __name__ == "__main__":
    unittest.main()
