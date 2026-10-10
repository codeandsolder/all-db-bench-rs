from __future__ import annotations

import importlib.util
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


if __name__ == "__main__":
    unittest.main()
