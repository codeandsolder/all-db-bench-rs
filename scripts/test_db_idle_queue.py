from __future__ import annotations

import hashlib
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("run-db-idle-queue.py")
SPEC = importlib.util.spec_from_file_location("run_db_idle_queue", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class DbIdleQueueTests(unittest.TestCase):
    def test_sha256(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "bin"
            path.write_bytes(b"benchmark")
            self.assertEqual(MODULE.sha256(path), hashlib.sha256(b"benchmark").hexdigest())

    def test_verify_pinned_binaries_checks_only_legacy_kv_pin(self) -> None:
        path = Path("/kv")
        with (
            patch.object(MODULE, "KV_BIN", path),
            patch.object(MODULE, "sha256", return_value=MODULE.KV_SHA) as digest,
        ):
            MODULE.verify_pinned_binaries()
        digest.assert_called_once_with(path)

    def test_verify_pinned_binaries_rejects_mismatch(self) -> None:
        with patch.object(MODULE, "sha256", return_value="wrong"):
            with self.assertRaises(RuntimeError):
                MODULE.verify_pinned_binaries()


if __name__ == "__main__":
    unittest.main()
