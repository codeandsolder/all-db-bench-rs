from __future__ import annotations

import importlib.util
import math
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("summarize-compression.py")
SPEC = importlib.util.spec_from_file_location("summarize_compression", MODULE_PATH)
assert SPEC and SPEC.loader
M = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(M)


def row(codec: str, *, throughput: float, cpu_ns: int, allocated: int, sectors_written: int) -> dict:
    return {
        "engine": "turbokv",
        "engine_version": "0.6.0",
        "engine_compression": codec,
        "workload": "write-burst",
        "durability": "sync",
        "records": 100,
        "ops_requested": 20,
        "ops_completed": 20,
        "writes": 20,
        "value_bytes": 4096,
        "value_pattern": "structured-text",
        "key_bytes": 8,
        "write_pattern": "append",
        "trial": 1,
        "ops_per_s": throughput,
        "read_latency": {"count": 0, "p99_us": 0},
        "write_txn_latency": {"count": 1, "p99_us": 123},
        "measured_process": {
            "cpu_runtime_ns": cpu_ns,
            "read_bytes": 0,
            "write_bytes": 8192,
        },
        "post_workload_settle": {"requested_ms": 1000},
        "compression_storage": {
            "footprint": {"apparent_bytes": allocated * 2, "allocated_block_bytes": allocated},
            "storage_counters": {
                "before": {"devices": [{"path": "/dev/test", "stat": [0, 0, 10, 0, 0, 0, 20]}]},
                "after": {"devices": [{"path": "/dev/test", "stat": [0, 0, 14, 0, 0, 0, 20 + sectors_written]}]},
            },
        },
    }


class CompressionSummaryTests(unittest.TestCase):
    def test_codec_ratios_use_matching_none_case(self) -> None:
        groups = M.summarize([
            row("none", throughput=100.0, cpu_ns=20_000, allocated=1_000_000, sectors_written=100),
            row("lz4", throughput=125.0, cpu_ns=25_000, allocated=500_000, sectors_written=40),
        ])
        self.assertEqual(len(groups), 2)
        lz4 = next(x for x in groups if x["engine_compression"] == "lz4")
        self.assertAlmostEqual(lz4["vs_none"]["throughput_ratio"], 1.25)
        self.assertAlmostEqual(lz4["vs_none"]["cpu_ns_per_op_ratio"], 1.25)
        self.assertAlmostEqual(lz4["vs_none"]["allocated_block_bytes_ratio"], 0.5)
        self.assertAlmostEqual(lz4["vs_none"]["device_write_bytes_ratio"], 0.4)

    def test_device_sector_accounting_uses_512_byte_units(self) -> None:
        read_bytes, write_bytes = M.device_bytes(row(
            "none", throughput=1.0, cpu_ns=1, allocated=1, sectors_written=7
        ))
        self.assertEqual(read_bytes, 4 * 512)
        self.assertEqual(write_bytes, 7 * 512)

    def test_logical_live_payload_counts_appended_records(self) -> None:
        value = M.logical_live_bytes(row(
            "none", throughput=1.0, cpu_ns=1, allocated=1, sectors_written=0
        ))
        self.assertEqual(value, 120 * (4096 + 8))
        self.assertTrue(math.isfinite(value))

    def test_zfs_uses_dataset_deltas_not_st_blocks(self) -> None:
        none = row("none", throughput=100.0, cpu_ns=20_000, allocated=1536, sectors_written=100)
        zstd = row("zstd", throughput=90.0, cpu_ns=24_000, allocated=512, sectors_written=70)
        for item, used_delta, logical_delta in [(none, 1_000_000, 2_000_000), (zstd, 500_000, 1_200_000)]:
            item["compression_storage"]["footprint"]["allocated_block_bytes_reliable"] = False
            item["compression_storage"]["dataset_snapshot"] = {
                "before": {"zfs_dataset": {"used": 10_000_000, "logicalused": 20_000_000}},
                "after": {"zfs_dataset": {
                    "used": 10_000_000 + used_delta,
                    "logicalused": 20_000_000 + logical_delta,
                }},
            }

        groups = M.summarize([none, zstd])
        compressed = next(x for x in groups if x["engine_compression"] == "zstd")
        self.assertFalse(compressed["db_allocated_block_bytes_reliable"])
        self.assertIsNone(compressed["db_allocated_block_bytes_median"])
        self.assertIsNone(compressed["logical_live_per_allocated_byte_median"])
        self.assertIsNone(compressed["vs_none"]["allocated_block_bytes_ratio"])
        self.assertAlmostEqual(compressed["vs_none"]["zfs_dataset_used_ratio"], 0.5)
        self.assertAlmostEqual(compressed["vs_none"]["zfs_dataset_logicalused_ratio"], 0.6)


if __name__ == "__main__":
    unittest.main()
