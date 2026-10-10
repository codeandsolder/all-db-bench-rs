from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("finalize-kv-baseline-admission-repairs.py")
SPEC = importlib.util.spec_from_file_location("finalize_kv_baseline_admission_repairs", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


def row(engine: str, trial: int, rate: float = 100.0) -> dict:
    return {
        "engine": engine,
        "durability": "relaxed",
        "workload": "write-burst",
        "records": 100_000,
        "ops_requested": 50_000,
        "trial": trial,
        "elapsed_s": 1.0,
        "ops_per_s": rate,
    }


class FinalizeKvBaselineAdmissionRepairsTests(unittest.TestCase):
    def test_replaces_only_targeted_group_and_records_v2_provenance(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repair_repo = root / "repo"
            base_rows = [row("bad", 1), row("bad", 2), row("bad", 3), row("clean", 1), row("clean", 2), row("clean", 3)]
            base_manifest = {
                "complete": True,
                "sources": [
                    {
                        "engine": "bad", "durability": "relaxed", "workload": "write-burst",
                        "final_status": "accepted", "run_id": "legacy", "rejected_pressure_attempts": 2,
                        "ops_requested": 50_000, "trials": 3,
                    },
                    {
                        "engine": "clean", "durability": "relaxed", "workload": "write-burst",
                        "final_status": "accepted", "source": "stock", "rejected_pressure_attempts": 0,
                        "ops_requested": 50_000, "trials": 3,
                    },
                ],
                "selected_followup_rejected_pressure_attempts": 2,
            }
            group = {
                "engine": "bad", "durability": "relaxed", "workload": "write-burst",
                "records": 100_000, "suggested_effective_ops": 50_000, "suggested_trials": 3,
                "legacy_run_id": "legacy", "legacy_rejected_pressure_attempts": 2,
            }
            plan = {
                "admission_repair_policy_version": 1,
                "source_manifest_sha256": "manifest",
                "suspect_source_count": 1,
                "groups": [group],
            }
            prefix = "fresh"
            rid = M.run_id(group, prefix)
            run = repair_repo / "results" / "runs" / rid
            run.mkdir(parents=True)
            fresh = [row("bad", 1, 110.0), row("bad", 2, 111.0), row("bad", 3, 109.0)]
            (run / "results.ndjson").write_text("".join(json.dumps(item) + "\n" for item in fresh))
            support = {
                "admission_policy": M.DEFAULT_ADMISSION_POLICY,
                "benchmark_binary_sha256": "bin",
                "lane": "kv", "profile": "quick", "trials": 3, "records": 100_000,
                "noise_guard_sha256": "noise", "runner_sha256": "runner",
                "initial_min_free_gib": 10, "case_min_free_gib": "10",
            }
            (run / "support.json").write_text(json.dumps(support))
            (run / "summary.json").write_text(json.dumps({"row_count": 3, "group_count": 1, "problems": []}))
            thresholds = {
                "read_only_workloads": ["point-read", "range-scan"],
                "read_only_min_seconds": 2.0,
                "stateful_min_total_seconds": 0.75,
                "max_cv": 0.1,
                "max_relative_spread": 0.25,
            }

            final_rows, manifest = M.finalize(
                base_rows=base_rows,
                base_manifest=base_manifest,
                base_manifest_sha256="manifest",
                base_results_sha256="results",
                plan=plan,
                plan_sha256="plan",
                repair_repo=repair_repo,
                run_prefix=prefix,
                thresholds=thresholds,
                expected_admission_policy=M.DEFAULT_ADMISSION_POLICY,
                expected_bench_sha256="bin",
            )
            self.assertEqual(len(final_rows), len(base_rows))
            self.assertEqual([r["ops_per_s"] for r in final_rows if r["engine"] == "bad"], [110.0, 111.0, 109.0])
            self.assertEqual([r["ops_per_s"] for r in final_rows if r["engine"] == "clean"], [100.0, 100.0, 100.0])
            self.assertEqual(manifest["selection_version"], 5)
            self.assertEqual(manifest["selected_followup_rejected_pressure_attempts"], 0)
            self.assertEqual(manifest["legacy_repaired_rejected_pressure_attempts"], 2)
            repaired = manifest["repaired_groups"][0]
            self.assertEqual(repaired["noise_guard_sha256"], "noise")
            self.assertEqual(repaired["initial_min_free_gib"], 10)
            self.assertEqual(repaired["case_min_free_gib"], "10")
            self.assertEqual(manifest["repair_initial_min_free_gib"], 10)
            self.assertEqual(manifest["repair_case_min_free_gib"], "10")
            self.assertEqual(manifest["sources"][0]["source"], "admission-v2-repair")
            self.assertEqual(manifest["sources"][0]["initial_min_free_gib"], 10)
            self.assertEqual(manifest["sources"][0]["case_min_free_gib"], "10")

    def test_fails_closed_on_wrong_admission_policy(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            group = {
                "engine": "bad", "durability": "relaxed", "workload": "write-burst",
                "records": 100_000, "suggested_effective_ops": 50_000, "suggested_trials": 1,
                "legacy_run_id": "legacy", "legacy_rejected_pressure_attempts": 1,
            }
            rid = M.run_id(group, "fresh")
            run = root / "results" / "runs" / rid
            run.mkdir(parents=True)
            (run / "results.ndjson").write_text(json.dumps(row("bad", 1)) + "\n")
            (run / "support.json").write_text(json.dumps({
                "admission_policy": "old", "benchmark_binary_sha256": "bin",
                "lane": "kv", "profile": "quick", "trials": 1, "records": 100_000,
                "noise_guard_sha256": "n", "runner_sha256": "r",
            }))
            (run / "summary.json").write_text(json.dumps({"row_count": 1, "group_count": 1, "problems": []}))
            with self.assertRaisesRegex(ValueError, "wrong admission policy"):
                M.finalize(
                    base_rows=[row("bad", 1)],
                    base_manifest={"complete": True, "sources": [{
                        "engine": "bad", "durability": "relaxed", "workload": "write-burst",
                        "final_status": "accepted", "rejected_pressure_attempts": 1,
                    }]},
                    base_manifest_sha256="m",
                    base_results_sha256="r",
                    plan={"admission_repair_policy_version": 1, "source_manifest_sha256": "m", "suspect_source_count": 1, "groups": [group]},
                    plan_sha256="p",
                    repair_repo=root,
                    run_prefix="fresh",
                    thresholds={"read_only_workloads": [], "read_only_min_seconds": 2, "stateful_min_total_seconds": 0.1, "max_cv": 1, "max_relative_spread": 1},
                    expected_admission_policy=M.DEFAULT_ADMISSION_POLICY,
                    expected_bench_sha256="bin",
                )

    def test_fails_closed_on_missing_free_space_provenance(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            group = {
                "engine": "bad", "durability": "relaxed", "workload": "write-burst",
                "records": 100_000, "suggested_effective_ops": 50_000, "suggested_trials": 1,
                "legacy_run_id": "legacy", "legacy_rejected_pressure_attempts": 1,
            }
            rid = M.run_id(group, "fresh")
            run = root / "results" / "runs" / rid
            run.mkdir(parents=True)
            (run / "results.ndjson").write_text(json.dumps(row("bad", 1)) + "\n")
            (run / "support.json").write_text(json.dumps({
                "admission_policy": M.DEFAULT_ADMISSION_POLICY, "benchmark_binary_sha256": "bin",
                "lane": "kv", "profile": "quick", "trials": 1, "records": 100_000,
                "noise_guard_sha256": "n", "runner_sha256": "r",
            }))
            (run / "summary.json").write_text(json.dumps({"row_count": 1, "group_count": 1, "problems": []}))
            with self.assertRaisesRegex(ValueError, "free-space provenance"):
                M.finalize(
                    base_rows=[row("bad", 1)],
                    base_manifest={"complete": True, "sources": [{
                        "engine": "bad", "durability": "relaxed", "workload": "write-burst",
                        "final_status": "accepted", "rejected_pressure_attempts": 1,
                    }]},
                    base_manifest_sha256="m", base_results_sha256="r",
                    plan={"admission_repair_policy_version": 1, "source_manifest_sha256": "m", "suspect_source_count": 1, "groups": [group]},
                    plan_sha256="p", repair_repo=root, run_prefix="fresh",
                    thresholds={"read_only_workloads": [], "read_only_min_seconds": 2, "stateful_min_total_seconds": 0.1, "max_cv": 1, "max_relative_spread": 1},
                    expected_admission_policy=M.DEFAULT_ADMISSION_POLICY, expected_bench_sha256="bin",
                )


if __name__ == "__main__":
    unittest.main()
