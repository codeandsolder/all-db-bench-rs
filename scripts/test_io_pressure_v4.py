from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("summarize_io_pressure", SCRIPT_DIR / "summarize-io-pressure.py")
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
load_case = MODULE.load_case


class ProtocolV4PressureSidecarTests(unittest.TestCase):
    def test_two_directional_jobs_are_joined(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            case = root / "case.json"
            pressure = root / "pressure.json"
            case.write_text(json.dumps({
                "format_version": 5,
                "scenario": "io-pressure-30pct",
                "engine": "redb",
                "engine_version": "test",
                "durability": "sync",
                "workload": "point-read",
                "records": 100,
                "ops_requested": 100,
                "ops_completed": 100,
                "value_bytes": 256,
                "txn_size": 100,
                "trial": 1,
                "ops_per_s": 1000.0,
                "read_latency": {"p99_us": 10.0},
                "write_txn_latency": {"p99_us": 0.0},
                "measured_process": {"cpu_runtime_ns": 1000, "runqueue_wait_fraction_of_wall": 0.0},
                "measured_system_delta": {"accounting_wall_ns": 1_000_000_000, "psi_io_full_us": 0},
            }))
            pressure.write_text(json.dumps({
                "pressure_percent": 30,
                "target_iops": 300,
                "target_read_iops": 210,
                "target_write_iops": 90,
                "baseline_iops": 1000,
                "baseline_read_iops": 700,
                "baseline_write_iops": 300,
                "read_fraction": 0.7,
                "write_fraction": 0.3,
                "bs_bytes": 131072,
                "jobs": [
                    {
                        "error": 0,
                        "job options": {"rw": "randread", "bs": "131072", "ba": "131072", "ioengine": "psync", "direct": "1"},
                        "read": {"iops": 200.0, "bw_bytes": 20_000_000, "io_bytes": 2_000_000},
                        "write": {"iops": 0.0, "bw_bytes": 0, "io_bytes": 0},
                    },
                    {
                        "error": 0,
                        "job options": {"rw": "randwrite", "bs": "131072", "ba": "131072", "ioengine": "psync", "direct": "1"},
                        "read": {"iops": 0.0, "bw_bytes": 0, "io_bytes": 0},
                        "write": {"iops": 80.0, "bw_bytes": 8_000_000, "io_bytes": 800_000},
                    },
                ],
            }))
            row = load_case(case, pressure, 1000.0, 131072, 700.0, 300.0, 0.7, 0.3)
            self.assertEqual(row["pressure_delivered_iops"], 280.0)
            self.assertAlmostEqual(row["pressure_delivered_vs_target"], 280.0 / 300.0)
            self.assertAlmostEqual(row["pressure_delivered_read_vs_target"], 200.0 / 210.0)
            self.assertAlmostEqual(row["pressure_delivered_write_vs_target"], 80.0 / 90.0)

    def test_rejects_single_mixed_job(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            case = root / "case.json"
            pressure = root / "pressure.json"
            case.write_text(json.dumps({"scenario": "io-pressure-30pct"}))
            pressure.write_text(json.dumps({
                "pressure_percent": 30,
                "target_iops": 300,
                "target_read_iops": 210,
                "target_write_iops": 90,
                "baseline_iops": 1000,
                "baseline_read_iops": 700,
                "baseline_write_iops": 300,
                "read_fraction": 0.7,
                "write_fraction": 0.3,
                "bs_bytes": 131072,
                "jobs": [{"error": 0, "job options": {"rw": "randrw"}}],
            }))
            with self.assertRaisesRegex(ValueError, "expected 2"):
                load_case(case, pressure, 1000.0, 131072, 700.0, 300.0, 0.7, 0.3)


if __name__ == "__main__":
    unittest.main()
