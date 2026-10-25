from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).with_name("verify-kv-ready.py")
SPEC = importlib.util.spec_from_file_location("verify_kv_ready", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class VerifyKvReadyTests(unittest.TestCase):
    def make_fixture(self, root: Path) -> tuple[Path, Path, Path, str]:
        repo = root / "repo"
        selected = root / "selected"
        binary_dir = root / "bin"
        (repo / "scripts").mkdir(parents=True)
        (repo / "tools" / "benchctl" / "src").mkdir(parents=True)
        (repo / "tools" / "benchctl" / "src" / "noise.rs").write_text("native noise policy\n")
        selected.mkdir()
        binary_dir.mkdir()
        for name in (
            "run-kv-matrix.sh",
            "kv-matrix-policy.sh",
            "performance-runner-common.sh",
            "continuous-noise-runner-common.sh",
        ):
            (repo / "scripts" / name).write_text(name + "\n")
        binary = binary_dir / "kvbench"
        binary.write_bytes(b"benchmark")
        binary_sha = MODULE.sha256(binary)
        commit = "a" * 40
        binary_manifest = binary_dir / "manifest.json"
        binary_manifest.write_text(json.dumps({"repo_commit": commit, "kv": {"path": str(binary), "sha256": binary_sha}}))

        support = {
            "admission_policy": MODULE.CONTINUOUS_ADMISSION_POLICY,
            "benchmark_binary_sha256": binary_sha,
            "build_profile": "external",
            "initial_min_free_gib": 10,
            "case_min_free_gib": "10",
            "continuous_noise_sample_ms": 250,
            "continuous_noise_max_cpu_percent": 50,
            "continuous_noise_max_io_average_mib_s": 2,
            "continuous_noise_max_io_rate_mib_s": 8,
            "hostname": "acer",
            "machine_id_sha256": "machine",
            "filesystem": "zfs",
            "source": "pool/dataset",
        }
        fields = {
            "runner_sha256": "run-kv-matrix.sh",
            "kv_matrix_policy_sha256": "kv-matrix-policy.sh",
            "performance_common_sha256": "performance-runner-common.sh",
            "continuous_noise_common_sha256": "continuous-noise-runner-common.sh",
        }
        for field, filename in fields.items():
            support[field] = MODULE.sha256(repo / "scripts" / filename)
        support["noise_guard_sha256"] = MODULE.sha256(repo / "tools" / "benchctl" / "src" / "noise.rs")
        support["continuous_noise_guard_sha256"] = support["noise_guard_sha256"]
        for run_id in ("stock", "resize"):
            run = repo / "results" / "runs" / run_id
            run.mkdir(parents=True)
            (run / "support.json").write_text(json.dumps(support))

        rows = [{"engine": "x", "trial": 1}]
        (selected / "results.ndjson").write_text("\n".join(json.dumps(row) for row in rows) + "\n")
        (selected / "summary.json").write_text(json.dumps({"row_count": 1, "group_count": 180, "problems": []}))
        (selected / "manifest.json").write_text(json.dumps({
            "complete": True,
            "selected_groups": 180,
            "selected_rows": 1,
            "selected_stock_groups": 179,
            "selected_resize_groups": 1,
            "selected_quality_repair_groups": 0,
            "final_status_counts": {"accepted": 180, "undersized": 0},
            "sources": [{"source": "resize", "run_id": "resize"}],
        }))
        return repo, selected, binary_manifest, commit

    def test_verify_accepts_mixed_fresh_sources_with_one_identity(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo, selected, binary_manifest, commit = self.make_fixture(Path(tmp))
            with mock.patch.object(MODULE, "checked_runner_commit", return_value=commit):
                ready = MODULE.verify(selected, binary_manifest, commit, repo, "stock", MODULE.CONTINUOUS_ADMISSION_POLICY)
            self.assertEqual(ready["ready_version"], 7)
            self.assertEqual(ready["group_count"], 180)
            self.assertEqual(set(ready["source_support_sha256"]), {"stock", "resize"})

    def test_verify_rejects_support_from_different_binary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo, selected, binary_manifest, commit = self.make_fixture(Path(tmp))
            support_path = repo / "results" / "runs" / "resize" / "support.json"
            support = json.loads(support_path.read_text())
            support["benchmark_binary_sha256"] = "wrong"
            support_path.write_text(json.dumps(support))
            with mock.patch.object(MODULE, "checked_runner_commit", return_value=commit):
                with self.assertRaisesRegex(ValueError, "benchmark binary mismatch"):
                    MODULE.verify(selected, binary_manifest, commit, repo, "stock", MODULE.CONTINUOUS_ADMISSION_POLICY)

    def test_selected_run_ids_rejects_manifest_count_mismatch(self) -> None:
        with self.assertRaisesRegex(ValueError, "source-run provenance mismatch"):
            MODULE.selected_run_ids({"selected_stock_groups": 1, "selected_resize_groups": 1, "sources": []}, "stock")


if __name__ == "__main__":
    unittest.main()
