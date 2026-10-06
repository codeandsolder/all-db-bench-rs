from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("summarize-tsdb.py")


def case(engine: str, trial: int, scale: float) -> dict[str, object]:
    return {
        "lane": "tsdb-server",
        "engine": engine,
        "engine_version": "1.0.0",
        "transport": "test-transport",
        "trial": trial,
        "total_samples": 100,
        "ingest": {"samples_per_s": 1000.0 * scale},
        "server_process": {"cpu_runtime_ns": 10_000 * scale, "write_bytes": 2_000 * scale},
        "server_data_bytes": 5_000 * scale,
        "server_startup_s": 0.5 * scale,
        "queries": {
            "point": {"latency": {"p99_us": 10.0 * scale}},
            "range": {"latency": {"p99_us": 20.0 * scale}},
            "aggregate": {"latency": {"p99_us": 30.0 * scale}},
        },
    }


class TsdbSummaryTests(unittest.TestCase):
    def test_medians_and_expected_trials(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cases = root / "cases"
            cases.mkdir()
            for trial, scale in ((1, 1.0), (2, 2.0), (3, 3.0)):
                (cases / f"t{trial}-test.json").write_text(json.dumps(case("test", trial, scale)))
            out = root / "summary.json"
            md = root / "summary.md"
            proc = subprocess.run(
                ["uv", "run", "--script", str(SCRIPT), str(root), "--expect-trials", "3", "--json-out", str(out), "--markdown-out", str(md)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            summary = json.loads(out.read_text())
            self.assertEqual(summary["problems"], [])
            row = summary["rows"][0]
            self.assertEqual(row["median_ingest_samples_per_s"], 2000.0)
            self.assertEqual(row["median_server_cpu_ns_per_sample"], 200.0)
            self.assertEqual(row["median_data_bytes_per_sample"], 100.0)
            self.assertIn("TSDB server benchmark summary", md.read_text())

    def test_missing_trial_is_validation_problem(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cases = root / "cases"
            cases.mkdir()
            (cases / "t1-test.json").write_text(json.dumps(case("test", 1, 1.0)))
            proc = subprocess.run(
                ["uv", "run", "--script", str(SCRIPT), str(root), "--expect-trials", "2"],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 1)
            self.assertIn("expected 2 trials, got 1", proc.stdout)


if __name__ == "__main__":
    unittest.main()
