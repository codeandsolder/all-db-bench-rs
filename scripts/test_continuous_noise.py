from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path("/srv/scratch/db-bench-work/record-read-semantics/scripts/run-with-continuous-noise.py")
SPEC = importlib.util.spec_from_file_location("run_with_continuous_noise", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


class ContinuousNoiseTests(unittest.TestCase):
    def test_descendants_closes_over_process_tree(self) -> None:
        meta = {
            10: (1, 0, 0, 100),
            11: (10, 0, 0, 101),
            12: (11, 0, 0, 102),
            20: (1, 0, 0, 200),
        }
        self.assertEqual(M.descendants(meta, {10}), {10, 11, 12})

    def test_contamination_thresholds(self) -> None:
        self.assertEqual(
            M.contamination_reasons(
                average_io_rate_bytes_s=1024.0,
                peak_io_rate_bytes_s=1024.0,
                peak_cpu_percent=10.0,
                max_io_average_mib_s=1.0,
                max_io_rate_mib_s=1.0,
                max_cpu_percent=50.0,
            ),
            [],
        )
        self.assertEqual(
            M.contamination_reasons(
                average_io_rate_bytes_s=2 * 1024 * 1024,
                peak_io_rate_bytes_s=2 * 1024 * 1024,
                peak_cpu_percent=50.0,
                max_io_average_mib_s=1.0,
                max_io_rate_mib_s=1.0,
                max_cpu_percent=50.0,
            ),
            ["foreign-io-average", "foreign-io-rate", "foreign-cpu"],
        )


if __name__ == "__main__":
    unittest.main()
