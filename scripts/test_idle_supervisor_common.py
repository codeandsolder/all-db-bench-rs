from __future__ import annotations

import errno
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import idle_supervisor_common as M


class IdleSupervisorCommonTests(unittest.TestCase):
    def test_waiting_heartbeat_enospc_is_nonfatal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "status.json"
            with mock.patch.object(Path, "write_text", side_effect=OSError(errno.ENOSPC, "full")):
                self.assertFalse(M.write_status(path, state="waiting-for-idle"))

    def test_nonwaiting_status_enospc_fails_hard(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "status.json"
            with mock.patch.object(Path, "write_text", side_effect=OSError(errno.ENOSPC, "full")):
                with self.assertRaises(OSError):
                    M.write_status(path, state="running")

    def test_storage_preflight_enforces_default_floor(self) -> None:
        with mock.patch("idle_supervisor_common.shutil.disk_usage") as disk_usage:
            disk_usage.return_value = mock.Mock(free=7 * 1024**3)
            with mock.patch.dict(os.environ, {}, clear=False):
                os.environ.pop("PERFORMANCE_MIN_FREE_GIB", None)
                self.assertEqual(M.storage_preflight(Path("/tmp")), 75)
            disk_usage.return_value = mock.Mock(free=9 * 1024**3)
            self.assertEqual(M.storage_preflight(Path("/tmp")), 0)

    def test_storage_floor_is_configurable_and_invalid_values_fail_closed(self) -> None:
        with mock.patch("idle_supervisor_common.shutil.disk_usage") as disk_usage:
            disk_usage.return_value = mock.Mock(free=2 * 1024**3)
            with mock.patch.dict(os.environ, {"PERFORMANCE_MIN_FREE_GIB": "1"}):
                self.assertEqual(M.storage_preflight(Path("/tmp")), 0)
            with mock.patch.dict(os.environ, {"PERFORMANCE_MIN_FREE_GIB": "-1"}):
                self.assertEqual(M.storage_preflight(Path("/tmp")), 2)
            with mock.patch.dict(os.environ, {"PERFORMANCE_MIN_FREE_GIB": "inf"}):
                self.assertEqual(M.storage_preflight(Path("/tmp")), 2)


if __name__ == "__main__":
    unittest.main()
