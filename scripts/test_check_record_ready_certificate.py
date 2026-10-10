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
ADMISSION = "pre-io+pre/post-external-v2"


def good() -> dict:
    h = "a" * 64
    return {
        "ready_version": 4,
        "repo_commit": BINARY,
        "binary_repo_commit": BINARY,
        "runner_repo_commit": "2" * 40,
        "admission_policy": ADMISSION,
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


class ReadyCertificateTests(unittest.TestCase):
    def test_good_certificate(self) -> None:
        M.validate_certificate(good(), expected_binary_commit=BINARY, expected_admission_policy=ADMISSION)

    def test_v3_is_rejected(self) -> None:
        r = good(); r["ready_version"] = 3
        with self.assertRaisesRegex(ValueError, "too old"):
            M.validate_certificate(r, expected_binary_commit=BINARY, expected_admission_policy=ADMISSION)

    def test_wrong_binary_is_rejected(self) -> None:
        r = good(); r["binary_repo_commit"] = "3" * 40
        with self.assertRaisesRegex(ValueError, "binary commit"):
            M.validate_certificate(r, expected_binary_commit=BINARY, expected_admission_policy=ADMISSION)

    def test_wrong_shape_is_rejected(self) -> None:
        r = good(); r["row_count"] = 195
        with self.assertRaisesRegex(ValueError, "row count"):
            M.validate_certificate(r, expected_binary_commit=BINARY, expected_admission_policy=ADMISSION)


if __name__ == "__main__":
    unittest.main()
