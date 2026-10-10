from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check-record-ready-certificate.py")
SPEC = importlib.util.spec_from_file_location("check_record_ready_certificate", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)

BINARY = "1" * 40
ADMISSION_V2 = "pre-io+pre/post-external-v2"
ADMISSION_V3 = M.CONTINUOUS_ADMISSION_POLICY


def good_v2() -> dict:
    h = "a" * 64
    return {
        "ready_version": 4,
        "repo_commit": BINARY,
        "binary_repo_commit": BINARY,
        "runner_repo_commit": "2" * 40,
        "admission_policy": ADMISSION_V2,
        "read_materialization": "full-record-v1",
        "write_materialization": "no-return-v1",
        "group_count": 40,
        "row_count": 200,
        "selected_noise_guard_sha256": h,
        "results_sha256": h,
        "selection_manifest_sha256": h,
        "summary_sha256": h,
        "binary_manifest_sha256": h,
        "source_support_sha256": {f"run-{i}": h for i in range(40)},
    }


def good_v3() -> dict:
    ready = good_v2()
    ready.update(
        {
            "ready_version": 5,
            "admission_policy": ADMISSION_V3,
            "selected_continuous_noise_guard_sha256": "b" * 64,
            "continuous_noise_sample_ms": 250,
            "continuous_noise_max_cpu_percent": 50,
            "continuous_noise_max_io_average_mib_s": 2,
            "continuous_noise_max_io_rate_mib_s": 8,
        }
    )
    return ready


class ReadyCertificateTests(unittest.TestCase):
    def test_good_v2_certificate(self) -> None:
        M.validate_certificate(
            good_v2(), expected_binary_commit=BINARY, expected_admission_policy=ADMISSION_V2
        )

    def test_good_v3_certificate(self) -> None:
        M.validate_certificate(
            good_v3(), expected_binary_commit=BINARY, expected_admission_policy=ADMISSION_V3
        )

    def test_v3_policy_requires_v5_certificate(self) -> None:
        ready = good_v3()
        ready["ready_version"] = 4
        with self.assertRaisesRegex(ValueError, "too old"):
            M.validate_certificate(
                ready, expected_binary_commit=BINARY, expected_admission_policy=ADMISSION_V3
            )

    def test_v3_policy_requires_continuous_guard(self) -> None:
        ready = good_v3()
        ready.pop("selected_continuous_noise_guard_sha256")
        with self.assertRaisesRegex(ValueError, "continuous noise guard"):
            M.validate_certificate(
                ready, expected_binary_commit=BINARY, expected_admission_policy=ADMISSION_V3
            )

    def test_v3_is_rejected_for_v2_policy(self) -> None:
        ready = good_v2()
        ready["ready_version"] = 3
        with self.assertRaisesRegex(ValueError, "too old"):
            M.validate_certificate(
                ready, expected_binary_commit=BINARY, expected_admission_policy=ADMISSION_V2
            )

    def test_wrong_binary_is_rejected(self) -> None:
        ready = good_v2()
        ready["binary_repo_commit"] = "3" * 40
        with self.assertRaisesRegex(ValueError, "binary commit"):
            M.validate_certificate(
                ready, expected_binary_commit=BINARY, expected_admission_policy=ADMISSION_V2
            )

    def test_wrong_shape_is_rejected(self) -> None:
        ready = good_v2()
        ready["row_count"] = 195
        with self.assertRaisesRegex(ValueError, "row count"):
            M.validate_certificate(
                ready, expected_binary_commit=BINARY, expected_admission_policy=ADMISSION_V2
            )


if __name__ == "__main__":
    unittest.main()
