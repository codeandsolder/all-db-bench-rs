from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

from concurrency_support_provenance import CONTINUOUS_ADMISSION, git_head, sha256, support_identity

SCRIPT = Path(__file__).with_name("check-concurrency-support-identity.py")
SPEC = importlib.util.spec_from_file_location("check_concurrency_support_identity", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)
REPO = SCRIPT.parent.parent


def kv_support() -> dict:
    return {
        "lane": "kv-concurrency",
        "profile": "quick",
        "admission_policy": CONTINUOUS_ADMISSION,
        "runner_sha256": sha256(REPO / "scripts/run-kv-concurrency-plan.sh"),
        "concurrency_runner_common_sha256": sha256(REPO / "scripts/concurrency-runner-common.sh"),
        "continuous_noise_common_sha256": sha256(REPO / "scripts/continuous-noise-runner-common.sh"),
        "concurrency_policy_sha256": sha256(REPO / "scripts/concurrency-matrix-policy.sh"),
        "noise_guard_sha256": sha256(REPO / "scripts/check-external-noise.py"),
        "continuous_noise_guard_sha256": sha256(REPO / "scripts/run-with-continuous-noise.py"),
        "benchmark_binary_sha256": "a" * 64,
        "build_profile": "external",
        "hostname": "test-host",
        "machine_id_sha256": "b" * 64,
        "filesystem": "zfs",
        "source": "testpool/scratch",
        "benchmark_source_commit": "c" * 40,
        "harness_commit": git_head(REPO),
        "continuous_noise_sample_ms": 250,
        "continuous_noise_max_cpu_percent": 50,
        "continuous_noise_max_io_average_mib_s": 2,
        "continuous_noise_max_io_rate_mib_s": 8,
        "initial_min_free_gib": 10,
        "case_min_free_gib": "10",
        "case_timeout_s": 600,
        "persy_lock_timeout_ms": 250,
        "prepared_db_protocol": "case-private-clean-close-v1",
    }


class ConcurrencySupportProvenanceTests(unittest.TestCase):
    def test_checker_accepts_exact_expected_identity(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            support = kv_support()
            identity = support_identity(
                support,
                expected_lane="kv-concurrency",
                repo=REPO,
                runner_name="run-kv-concurrency-plan.sh",
            )
            support_path = root / "support.json"
            plan_path = root / "plan.json"
            support_path.write_text(json.dumps(support))
            plan_path.write_text(json.dumps({"expected_measurement_identity": identity}))
            self.assertEqual(
                CHECK.verify(
                    support_path,
                    plan_path,
                    REPO,
                    runner="run-kv-concurrency-plan.sh",
                    lane="kv-concurrency",
                ),
                identity,
            )

    def test_checker_rejects_changed_expected_host(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            support = kv_support()
            identity = support_identity(
                support,
                expected_lane="kv-concurrency",
                repo=REPO,
                runner_name="run-kv-concurrency-plan.sh",
            )
            expected = dict(identity)
            expected["hostname"] = "different-host"
            support_path = root / "support.json"
            plan_path = root / "plan.json"
            support_path.write_text(json.dumps(support))
            plan_path.write_text(json.dumps({"expected_measurement_identity": expected}))
            with self.assertRaisesRegex(ValueError, "measurement identity differs"):
                CHECK.verify(
                    support_path,
                    plan_path,
                    REPO,
                    runner="run-kv-concurrency-plan.sh",
                    lane="kv-concurrency",
                )

    def test_support_rejects_runtime_helper_hash_drift(self) -> None:
        support = kv_support()
        support["continuous_noise_common_sha256"] = "f" * 64
        with self.assertRaisesRegex(ValueError, "runtime provenance mismatch"):
            support_identity(
                support,
                expected_lane="kv-concurrency",
                repo=REPO,
                runner_name="run-kv-concurrency-plan.sh",
            )


if __name__ == "__main__":
    unittest.main()
