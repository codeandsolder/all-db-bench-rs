from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("audit-concurrency-sizing.py")
SPEC = importlib.util.spec_from_file_location("audit_concurrency_sizing", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


def row(*, workload: str, clients: int, trial: int, elapsed: float, rate: float, engine: str = "fast") -> dict:
    ops = 200_000 if workload == "point-read" else 1_000 if workload in {"range-scan", "tiny-txn"} else 50_000
    return {
        "scenario": "concurrency-primary",
        "engine": engine,
        "durability": "sync",
        "workload": workload,
        "clients": clients,
        "trial": trial,
        "records": 100_000,
        "ops_requested": ops,
        "ops_completed": ops,
        "elapsed_s": elapsed,
        "ops_per_s": rate,
    }


class ConcurrencySizingTests(unittest.TestCase):
    def make_run(self, jobs: list[str], rows: list[dict]) -> Path:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        run = Path(tmp.name)
        (run / "cases").mkdir()
        (run / "support.json").write_text(json.dumps({
            "lane": "kv-concurrency", "profile": "quick", "trials": 3,
            "records": 100_000, "default_ops": 50_000,
        }))
        (run / "jobs.txt").write_text("\n".join(jobs) + "\n")
        for i, item in enumerate(rows):
            (run / "cases" / f"r{i}.json").write_text(json.dumps(item))
        return run

    def test_missing_group_becomes_one_probe_case(self) -> None:
        jobs = [f"primary|fast|sync|point-read|{c}|{t}" for t in (1,2,3) for c in (1,8)]
        run = self.make_run(jobs, [row(workload="point-read", clients=1, trial=2, elapsed=1.0, rate=200_000)])
        report = M.audit(run)
        self.assertEqual(report["planned_client_group_count"], 2)
        self.assertEqual(report["observed_client_group_count"], 1)
        self.assertEqual(report["missing_probe_case_count"], 1)
        self.assertEqual(report["missing_probe_cases"][0]["clients"], 8)
        self.assertEqual(report["missing_probe_cases"][0]["trial"], 1)

    def test_additional_run_fills_missing_client_group(self) -> None:
        jobs = [f"primary|fast|sync|point-read|{c}|{t}" for t in (1,2,3) for c in (1,8)]
        stock = self.make_run(jobs, [row(workload="point-read", clients=1, trial=1, elapsed=1.0, rate=200_000)])
        extra_tmp = tempfile.TemporaryDirectory(); self.addCleanup(extra_tmp.cleanup)
        extra = Path(extra_tmp.name); (extra / "cases").mkdir()
        (extra / "cases" / "c8.json").write_text(json.dumps(row(workload="point-read", clients=8, trial=2, elapsed=.5, rate=400_000)))
        report = M.audit(stock, [extra])
        self.assertEqual(report["missing_probe_case_count"], 0)
        self.assertEqual(report["observed_client_group_count"], 2)
        self.assertEqual(report["additional_run_dirs"], [str(extra)])

    def test_fixed_keyspace_family_recommends_same_more_ops_after_complete_probe(self) -> None:
        jobs = [f"primary|fast|sync|point-read|{c}|{t}" for t in (1,2,3) for c in (1,8)]
        rows = [
            row(workload="point-read", clients=1, trial=1, elapsed=1.0, rate=200_000),
            row(workload="point-read", clients=8, trial=1, elapsed=.5, rate=400_000),
        ]
        report = M.audit(self.make_run(jobs, rows))
        fam = report["families"][0]
        self.assertEqual(fam["status"], "undersized")
        self.assertEqual(fam["recommendation"], "more-ops")
        self.assertEqual(fam["suggested_ops"], 2_000_000)

    def test_moderately_short_stateful_family_uses_fresh_trials(self) -> None:
        jobs = [f"primary|fast|sync|write-burst|{c}|{t}" for t in (1,2,3) for c in (1,8)]
        rows = [
            row(workload="write-burst", clients=1, trial=1, elapsed=.8, rate=62_500),
            row(workload="write-burst", clients=8, trial=1, elapsed=.5, rate=100_000),
        ]
        fam = M.audit(self.make_run(jobs, rows))["families"][0]
        self.assertEqual(fam["status"], "undersized")
        self.assertEqual(fam["recommendation"], "more-fresh-trials")
        self.assertEqual(fam["suggested_ops"], 50_000)
        self.assertEqual(fam["suggested_trials"], 6)

    def test_severe_append_family_requires_redesign(self) -> None:
        jobs = [f"primary|fast|sync|tiny-txn|{c}|{t}" for t in (1,2,3) for c in (1,8)]
        rows = [
            row(workload="tiny-txn", clients=1, trial=1, elapsed=.02, rate=50_000),
            row(workload="tiny-txn", clients=8, trial=1, elapsed=.004, rate=250_000),
        ]
        fam = M.audit(self.make_run(jobs, rows))["families"][0]
        self.assertEqual(fam["status"], "redesign-required")
        self.assertEqual(fam["recommendation"], "state-preserving-longer-window")
        self.assertGreater(fam["suggested_trials"], M.STATEFUL_MAX_TRIALS)

    def test_markdown_surfaces_provisional_stateful_redesign(self) -> None:
        jobs = [f"primary|fast|sync|tiny-txn|{c}|{t}" for t in (1,2,3) for c in (1,8)]
        rows = [
            row(workload="tiny-txn", clients=1, trial=1, elapsed=.02, rate=50_000),
        ]
        report = M.audit(self.make_run(jobs, rows))
        self.assertEqual(report["observed_stateful_redesign_risk_count"], 1)
        self.assertIn("severe enough to require redesign: **1**", M.markdown(report))

    def test_partial_ops_are_rejected(self) -> None:
        jobs = ["primary|fast|sync|point-read|1|1"]
        bad = row(workload="point-read", clients=1, trial=1, elapsed=1.0, rate=200_000)
        bad["ops_completed"] -= 1
        with self.assertRaisesRegex(ValueError, "partial result"):
            M.audit(self.make_run(jobs, [bad]))

    def test_expected_admission_policy_is_enforced(self) -> None:
        run = self.make_run(
            ["primary|fast|sync|point-read|1|1"],
            [row(workload="point-read", clients=1, trial=1, elapsed=1.0, rate=200_000)],
        )
        support_path = run / "support.json"
        support = json.loads(support_path.read_text())
        support["admission_policy"] = "pre-io+pre/post-external-v2"
        support_path.write_text(json.dumps(support))
        M.audit(run, expected_admission_policy="pre-io+pre/post-external-v2")
        with self.assertRaisesRegex(ValueError, "unexpected admission policy"):
            M.audit(run, expected_admission_policy="wrong-policy")

    def test_exact_plan_jobs_are_supported(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        run = Path(tmp.name)
        (run / "cases").mkdir()
        (run / "support.json").write_text(json.dumps({"lane": "kv-concurrency", "profile": "quick", "trials": 1}))
        (run / "jobs.txt").write_text("concurrency-primary|fast|sync|point-read|1|100000|200000|1|growth|append|0\n")
        (run / "cases" / "r0.json").write_text(json.dumps(row(workload="point-read", clients=1, trial=1, elapsed=1.0, rate=200_000)))
        report = M.audit(run)
        self.assertEqual(report["planned_case_count"], 1)
        self.assertEqual(report["observed_client_group_count"], 1)
        self.assertEqual(report["families"][0]["current_records"], 100_000)
        self.assertEqual(report["families"][0]["current_ops"], 200_000)


if __name__ == "__main__":
    unittest.main()
