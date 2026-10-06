from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("merge-tsdb-result.py")


class TsdbMergeTests(unittest.TestCase):
    def test_server_deltas_and_data_size(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            client = root / "client.json"
            before = root / "before.json"
            after = root / "after.json"
            data = root / "data"
            data.mkdir()
            (data / "a").write_bytes(b"x" * 123)
            client.write_text(json.dumps({"lane": "tsdb-server", "engine": "test"}))
            before.write_text(
                json.dumps(
                    {
                        "captured_monotonic_ns": 1_000,
                        "cpu_runtime_ns": 100,
                        "runqueue_wait_ns": 20,
                        "write_bytes": 1_000,
                        "rss_kib": 10,
                        "peak_rss_kib": 12,
                        "threads": 2,
                        "pids": [1],
                    }
                )
            )
            after.write_text(
                json.dumps(
                    {
                        "captured_monotonic_ns": 2_000,
                        "cpu_runtime_ns": 500,
                        "runqueue_wait_ns": 120,
                        "write_bytes": 1_500,
                        "rss_kib": 14,
                        "peak_rss_kib": 16,
                        "threads": 3,
                        "pids": [1, 2],
                    }
                )
            )
            out = root / "out.json"
            proc = subprocess.run(
                [
                    "uv",
                    "run",
                    "--script",
                    str(SCRIPT),
                    "--client",
                    str(client),
                    "--server-before",
                    str(before),
                    "--server-after",
                    str(after),
                    "--data-dir",
                    str(data),
                    "--startup-s",
                    "0.25",
                    "--output",
                    str(out),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            result = json.loads(out.read_text())
            self.assertEqual(result["server_process"]["cpu_runtime_ns"], 400)
            self.assertEqual(result["server_process"]["runqueue_wait_ns"], 100)
            self.assertEqual(result["server_process"]["write_bytes"], 500)
            self.assertEqual(result["server_process"]["accounting_wall_ns"], 1_000)
            self.assertEqual(result["server_process"]["rss_after_kib"], 14)
            self.assertEqual(result["server_data_bytes"], 123)
            self.assertEqual(result["server_startup_s"], 0.25)


if __name__ == "__main__":
    unittest.main()
