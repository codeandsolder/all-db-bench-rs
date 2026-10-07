from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("scrub-short-trial-pressure.py")
SPEC = importlib.util.spec_from_file_location("scrub_short_trial_pressure", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def row(*, trial: int, elapsed: float = 0.03, rq: float = 0.0, psi: float = 0.0, cpu: float = 1.0):
    wall_ns = int(elapsed * 1_000_000_000)
    return {
        "trial": trial,
        "engine": "rocksdb",
        "durability": "relaxed",
        "workload": "write-burst",
        "elapsed_s": elapsed,
        "ops_per_s": 1_000_000.0,
        "measured_process": {
            "cpu_runtime_fraction_of_wall": cpu,
            "runqueue_wait_fraction_of_wall": rq,
            "write_bytes": 1234,
        },
        "measured_system_delta": {
            "accounting_wall_ns": wall_ns,
            "psi_cpu_some_us": int(psi * wall_ns / 1000),
            "psi_io_full_us": int(0.9 * wall_ns / 1000),
        },
    }


class ShortTrialPressureTests(unittest.TestCase):
    def test_flags_short_runqueue_or_cpu_psi_pressure(self) -> None:
        self.assertIsNotNone(MODULE.pressure_evidence(row(trial=1, rq=0.031), max_elapsed_s=0.5, max_runqueue_fraction=0.03, max_cpu_psi_fraction=0.05, max_benchmark_cpu_share=0.5, cpu_count=8))
        self.assertIsNotNone(MODULE.pressure_evidence(row(trial=2, psi=0.051), max_elapsed_s=0.5, max_runqueue_fraction=0.03, max_cpu_psi_fraction=0.05, max_benchmark_cpu_share=0.5, cpu_count=8))

    def test_can_flag_open_interval_without_changing_default(self) -> None:
        sample = row(trial=9)
        open_elapsed = 0.02
        wall_ns = int(open_elapsed * 1_000_000_000)
        sample.update({
            "open_s": open_elapsed,
            "open_process": {
                "cpu_runtime_fraction_of_wall": 0.8,
                "runqueue_wait_fraction_of_wall": 0.2,
                "write_bytes": 0,
            },
            "open_system_delta": {
                "accounting_wall_ns": wall_ns,
                "psi_cpu_some_us": 0,
            },
        })
        kwargs = dict(max_elapsed_s=0.5, max_runqueue_fraction=0.03, max_cpu_psi_fraction=0.05, max_benchmark_cpu_share=0.5, cpu_count=8)
        self.assertIsNone(MODULE.pressure_evidence(sample, **kwargs))
        evidence = MODULE.pressure_evidence(sample, interval="open", **kwargs)
        self.assertIsNotNone(evidence)
        self.assertEqual(evidence["interval"], "open")

    def test_scrub_can_target_open_interval(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            (run / "cases").mkdir()
            sample = row(trial=10)
            open_elapsed = 0.02
            wall_ns = int(open_elapsed * 1_000_000_000)
            sample["open_s"] = open_elapsed
            sample["open_process"] = {
                "cpu_runtime_fraction_of_wall": 0.7,
                "runqueue_wait_fraction_of_wall": 0.08,
                "write_bytes": 0,
            }
            sample["open_system_delta"] = {"accounting_wall_ns": wall_ns, "psi_cpu_some_us": 0}
            case = run / "cases" / "t10-reopen.json"
            case.write_text(json.dumps(sample))
            report = MODULE.scrub(run, cpu_count=8, intervals=("open",))
            self.assertEqual(report["rejected"], 1)
            self.assertEqual(report["cases"][0]["interval"], "open")
            self.assertFalse(case.exists())

    def test_does_not_flag_long_or_high_parallel_or_io_only(self) -> None:
        kwargs = dict(max_elapsed_s=0.5, max_runqueue_fraction=0.03, max_cpu_psi_fraction=0.05, max_benchmark_cpu_share=0.5, cpu_count=8)
        self.assertIsNone(MODULE.pressure_evidence(row(trial=1, elapsed=0.6, rq=0.5, psi=0.5), **kwargs))
        self.assertIsNone(MODULE.pressure_evidence(row(trial=2, rq=0.5, psi=0.5, cpu=4.1), **kwargs))
        self.assertIsNone(MODULE.pressure_evidence(row(trial=3), **kwargs))

    def test_scrub_archives_evidence_and_invalidates_derived_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            for sub in ("cases", "noise", "stderr"):
                (run / sub).mkdir()
            good = run / "cases" / "t1-rocksdb-relaxed-write-burst.json"
            bad = run / "cases" / "t2-rocksdb-relaxed-write-burst.json"
            good.write_text(json.dumps(row(trial=1)))
            bad.write_text(json.dumps(row(trial=2, psi=0.2)))
            (run / "noise" / f"{bad.stem}.before.json").write_text("{}")
            (run / "noise" / f"{bad.stem}.ready.json").write_text("{}")
            (run / "noise" / f"{bad.stem}.after.json").write_text("{}")
            (run / "stderr" / f"{bad.stem}.prepare.log").write_text("prepare diagnostic")
            (run / "stderr" / f"{bad.stem}.log").write_text("diagnostic")
            for name in ("results.ndjson", "summary.json", "summary.md", "host-end.txt"):
                (run / name).write_text("stale")
            report = MODULE.scrub(run, cpu_count=8)
            self.assertEqual(report["rejected"], 1)
            self.assertTrue(good.exists())
            self.assertFalse(bad.exists())
            archive = Path(report["cases"][0]["archive"])
            self.assertTrue((archive / "case.json").is_file())
            self.assertTrue((archive / "noise-before.json").is_file())
            self.assertTrue((archive / "noise-ready.json").is_file())
            self.assertTrue((archive / "noise-after.json").is_file())
            self.assertTrue((archive / "prepare-stderr.log").is_file())
            self.assertTrue((archive / "stderr.log").is_file())
            self.assertTrue((archive / "rejection.json").is_file())
            for name in ("results.ndjson", "summary.json", "summary.md", "host-end.txt"):
                self.assertFalse((run / name).exists())

    def test_targeted_scrub_only_examines_named_case(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            (run / "cases").mkdir()
            first = run / "cases" / "t1-rocksdb-relaxed-write-burst.json"
            second = run / "cases" / "t2-rocksdb-relaxed-write-burst.json"
            first.write_text(json.dumps(row(trial=1, psi=0.2)))
            second.write_text(json.dumps(row(trial=2, psi=0.2)))
            report = MODULE.scrub(run, cpu_count=8, case_ids={first.stem})
            self.assertEqual(report["examined"], 1)
            self.assertEqual(report["rejected"], 1)
            self.assertFalse(first.exists())
            self.assertTrue(second.exists())

    def test_dry_run_preserves_case(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            (run / "cases").mkdir()
            bad = run / "cases" / "t1-rocksdb-relaxed-write-burst.json"
            bad.write_text(json.dumps(row(trial=1, rq=0.5)))
            report = MODULE.scrub(run, cpu_count=8, dry_run=True)
            self.assertEqual(report["rejected"], 1)
            self.assertTrue(bad.exists())
            self.assertFalse((run / "rejected-pressure").exists())


if __name__ == "__main__":
    unittest.main()
