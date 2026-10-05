from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from io_evidence import load_storage_delta


class StorageDeltaTests(unittest.TestCase):
    def _sidecar(self, before_stat: list[int], after_stat: list[int]) -> Path:
        tmp = tempfile.NamedTemporaryFile(mode="w", suffix=".json", delete=False)
        payload = {
            "before": {
                "filesystem": "zfs",
                "source": "pool/data",
                "captured_monotonic_ns": 1_000_000_000,
                "devices": [{"name": "disk0", "stat": before_stat}],
                "zfs_arc": {"hits": 10, "misses": 5},
                "zfs_pool_io": {"direct_read_count": 10, "direct_read_bytes": 1000, "direct_write_count": 20, "direct_write_bytes": 2000, "arc_read_count": 30, "arc_read_bytes": 3000, "arc_write_count": 40, "arc_write_bytes": 4000},
            },
            "after": {
                "filesystem": "zfs",
                "source": "pool/data",
                "captured_monotonic_ns": 2_000_000_000,
                "devices": [{"name": "disk0", "stat": after_stat}],
                "zfs_arc": {"hits": 14, "misses": 6},
                "zfs_pool_io": {"direct_read_count": 12, "direct_read_bytes": 1512, "direct_write_count": 23, "direct_write_bytes": 3024, "arc_read_count": 31, "arc_read_bytes": 3512, "arc_write_count": 42, "arc_write_bytes": 5024},
            },
        }
        json.dump(payload, tmp)
        tmp.close()
        self.addCleanup(Path(tmp.name).unlink, missing_ok=True)
        return Path(tmp.name)

    def test_in_flight_gauge_may_decrease(self) -> None:
        before = [100, 0, 200, 0, 300, 0, 400, 0, 7, 500, 600, 0, 0, 700, 0, 10, 0]
        after = [101, 0, 210, 0, 302, 0, 420, 0, 2, 503, 605, 0, 0, 700, 0, 11, 0]
        delta = load_storage_delta(self._sidecar(before, after))
        self.assertEqual(delta["storage_read_ios"], 1)
        self.assertEqual(delta["storage_read_bytes"], 10 * 512)
        self.assertEqual(delta["storage_write_ios"], 2)
        self.assertEqual(delta["storage_write_bytes"], 20 * 512)
        self.assertEqual(delta["storage_flushes"], 1)
        self.assertEqual(delta["zfs_arc_hits"], 4)
        self.assertEqual(delta["zfs_arc_misses"], 1)
        self.assertEqual(delta["zfs_direct_read_ios"], 2)
        self.assertEqual(delta["zfs_direct_read_bytes"], 512)
        self.assertEqual(delta["zfs_direct_write_ios"], 3)
        self.assertEqual(delta["zfs_direct_write_bytes"], 1024)

    def test_consumed_monotonic_counter_may_not_decrease(self) -> None:
        before = [100, 0, 200, 0, 300, 0, 400, 0, 1, 500, 600]
        after = [99, 0, 210, 0, 302, 0, 420, 0, 0, 503, 605]
        with self.assertRaisesRegex(ValueError, "block counters moved backwards"):
            load_storage_delta(self._sidecar(before, after))


if __name__ == "__main__":
    unittest.main()
