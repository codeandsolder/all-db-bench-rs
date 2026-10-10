from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("make-record-ready-v4-dropin.py")
SPEC = importlib.util.spec_from_file_location("make_record_ready_v4_dropin", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M

assert SPEC.loader is not None
SPEC.loader.exec_module(M)

class ReadyV4DropinTests(unittest.TestCase):
    def test_dropin_requires_v4_checker_with_exact_identity(self) -> None:
        runtime = Path("/x/runtime")
        text = M.dropin_text(runtime)
        self.assertIn("ExecStartPre=/usr/bin/uv run --script /x/runtime/scripts/check-record-ready-certificate.py", text)
        self.assertIn("--expected-binary-commit 01b8a5c4ecfd79e69f0c98cef823d7fe79401d25", text)
        self.assertIn("--expected-admission-policy pre-io+pre/post-external-v2", text)
        self.assertIn("--expected-groups 40 --expected-rows 200", text)
        self.assertNotIn("ExecStartPre=\n", text)

if __name__ == "__main__":
    unittest.main()
