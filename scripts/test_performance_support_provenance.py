from __future__ import annotations

import unittest
from pathlib import Path

from performance_support_provenance import (
    CONTINUOUS_ADMISSION,
    git_head,
    sha256,
    support_identity,
    verify_binary_manifest,
)

REPO = Path(__file__).resolve().parents[1]


def base_support(lane: str, runner: str, policy_key: str, policy_file: str, *, floor: int) -> dict:
    return {
        "lane": lane,
        "profile": "quick",
        "admission_policy": CONTINUOUS_ADMISSION,
        "runner_sha256": sha256(REPO / "scripts" / runner),
        "performance_common_sha256": sha256(REPO / "scripts/performance-runner-common.sh"),
        "continuous_noise_common_sha256": sha256(REPO / "scripts/continuous-noise-runner-common.sh"),
        "noise_guard_sha256": sha256(REPO / "scripts/check-external-noise.py"),
        "continuous_noise_guard_sha256": sha256(REPO / "scripts/run-with-continuous-noise.py"),
        policy_key: sha256(REPO / "scripts" / policy_file),
        "benchmark_binary_sha256": "a" * 64,
        "benchmark_source_commit": "b" * 40,
        "harness_commit": git_head(REPO),
        "build_profile": "external",
        "hostname": "test-host",
        "machine_id_sha256": "c" * 64,
        "filesystem": "zfs",
        "source": "testpool/scratch",
        "resume_order_policy": "reshuffle-remaining",
        "continuous_noise_sample_ms": 250,
        "continuous_noise_max_cpu_percent": 50,
        "continuous_noise_max_io_average_mib_s": 2,
        "continuous_noise_max_io_rate_mib_s": 8,
        "initial_min_free_gib": floor,
        "case_min_free_gib": str(floor),
    }


class PerformanceSupportProvenanceTests(unittest.TestCase):
    def test_kv_sustained_exact_identity_and_manifest(self) -> None:
        support = base_support(
            "kv-sustained", "run-kv-sustained-matrix.sh", "sustained_policy_sha256", "sustained-matrix-policy.sh", floor=20
        )
        identity = support_identity(
            support, expected_lane="kv-sustained", repo=REPO,
            runner_name="run-kv-sustained-matrix.sh", min_free_gib=20,
        )
        verify_binary_manifest(identity, {
            "kv": {"sha256": "a" * 64},
            "source_commits": {"kv": "b" * 40},
        })

    def test_exact_continuous_thresholds_are_required(self) -> None:
        support = base_support(
            "kv-sustained", "run-kv-sustained-matrix.sh", "sustained_policy_sha256", "sustained-matrix-policy.sh", floor=20
        )
        support["continuous_noise_max_cpu_percent"] = 51
        with self.assertRaisesRegex(ValueError, "unexpected continuous_noise_max_cpu_percent"):
            support_identity(
                support, expected_lane="kv-sustained", repo=REPO,
                runner_name="run-kv-sustained-matrix.sh", min_free_gib=20,
            )

    def test_helper_hash_drift_is_rejected(self) -> None:
        support = base_support(
            "kv-sustained", "run-kv-sustained-matrix.sh", "sustained_policy_sha256", "sustained-matrix-policy.sh", floor=20
        )
        support["continuous_noise_common_sha256"] = "f" * 64
        with self.assertRaisesRegex(ValueError, "runtime provenance mismatch"):
            support_identity(
                support, expected_lane="kv-sustained", repo=REPO,
                runner_name="run-kv-sustained-matrix.sh", min_free_gib=20,
            )

    def test_record_sustained_semantics_and_rocks_provenance_are_required(self) -> None:
        support = base_support(
            "record-sustained", "run-record-sustained-matrix.sh", "sustained_policy_sha256", "sustained-matrix-policy.sh", floor=20
        )
        support.update({
            "rocksdb_benchmark_binary_sha256": "d" * 64,
            "surrealdb_rocksdb_source_commit": "e" * 40,
            "rocks_build_profile": "external",
            "read_materialization": "full-record-v1",
            "write_materialization": "no-return-v1",
        })
        identity = support_identity(
            support, expected_lane="record-sustained", repo=REPO,
            runner_name="run-record-sustained-matrix.sh", min_free_gib=20,
        )
        manifest = {
            "record": {"sha256": "a" * 64},
            "record_rocksdb": {"sha256": "d" * 64},
            "source_commits": {"record": "b" * 40, "record_rocksdb": "e" * 40},
            "read_materialization": "full-record-v1",
            "write_materialization": "no-return-v1",
        }
        verify_binary_manifest(identity, manifest)
        support["write_materialization"] = "legacy-return-v0"
        with self.assertRaisesRegex(ValueError, "write_materialization"):
            support_identity(
                support, expected_lane="record-sustained", repo=REPO,
                runner_name="run-record-sustained-matrix.sh", min_free_gib=20,
            )

    def test_reopen_requires_warm_cache_and_ten_gib_floor(self) -> None:
        support = base_support(
            "kv-reopen", "run-reopen-matrix.sh", "kv_matrix_policy_sha256", "kv-matrix-policy.sh", floor=10
        )
        support["cache_mode"] = "warm"
        support_identity(
            support, expected_lane="kv-reopen", repo=REPO,
            runner_name="run-reopen-matrix.sh", min_free_gib=10, expected_cache_mode="warm",
        )
        support["cache_mode"] = "cold"
        with self.assertRaisesRegex(ValueError, "cache_mode"):
            support_identity(
                support, expected_lane="kv-reopen", repo=REPO,
                runner_name="run-reopen-matrix.sh", min_free_gib=10, expected_cache_mode="warm",
            )


if __name__ == "__main__":
    unittest.main()
