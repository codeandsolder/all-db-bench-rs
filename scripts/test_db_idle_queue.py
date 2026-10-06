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

    def test_verify_pinned_binaries_checks_all_three(self) -> None:
        paths = [Path("/kv"), Path("/record"), Path("/rocks")]
        with (
            patch.object(MODULE, "KV_BIN", paths[0]),
            patch.object(MODULE, "RECORD_BIN", paths[1]),
            patch.object(MODULE, "ROCKS_BIN", paths[2]),
            patch.object(
                MODULE,
                "sha256",
                side_effect=[MODULE.KV_SHA, MODULE.RECORD_SHA, MODULE.ROCKS_SHA],
            ) as digest,
        ):
            MODULE.verify_pinned_binaries()
        self.assertEqual([call.args[0] for call in digest.call_args_list], paths)

    def test_verify_pinned_binaries_rejects_mismatch(self) -> None:
        with patch.object(MODULE, "sha256", return_value="wrong"):
            with self.assertRaises(RuntimeError):
                MODULE.verify_pinned_binaries()


if __name__ == "__main__":
    unittest.main()
