from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("import-concurrency-results.py")
SPEC = importlib.util.spec_from_file_location("import_concurrency_results", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def write_case(run: Path, case_id: str, *, engine: str, quiet: bool = True) -> None:
    (run / "cases").mkdir(parents=True, exist_ok=True)
    (run / "noise").mkdir(parents=True, exist_ok=True)
    (run / "cases" / f"{case_id}.json").write_text(
        json.dumps({"engine": engine, "trial": 1, "workload": "delete-burst", "clients": 8, "elapsed_s": 1.5}) + "\n"
    )
    evidence = json.dumps({"quiet": quiet}) + "\n"
    (run / "noise" / f"{case_id}.before.json").write_text(evidence)
    (run / "noise" / f"{case_id}.after.json").write_text(evidence)


class ImportConcurrencyResultsTests(unittest.TestCase):
    def make_source(self, root: Path) -> Path:
        source = root / "v1"
        source.mkdir()
        (source / "support.json").write_text(
            json.dumps(
                {
                    "lane": "kv-concurrency",
                    "profile": "quick",
                    "benchmark_binary_sha256": "old-bin",
                    "runner_sha256": "old-runner",
                }
            )
            + "\n"
        )
        (source / "jobs.txt").write_text("job\n")
        return source

    def test_excludes_persy_and_hashes_quiet_cases(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = self.make_source(root)
            write_case(source, "redb-case", engine="redb")
            write_case(source, "persy-case", engine="persy")
            dest = root / "v2"
            manifest = MODULE.import_results(source, dest, {"persy"})
            self.assertEqual(manifest["imported_case_count"], 1)
            self.assertEqual([x["case_id"] for x in manifest["cases"]], ["redb-case"])
            self.assertTrue((dest / "cases" / "redb-case.json").is_file())
            self.assertFalse((dest / "cases" / "persy-case.json").exists())
            self.assertTrue((dest / "import-manifest.json").is_file())

    def test_rejects_nonquiet_source(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = self.make_source(root)
            write_case(source, "bad", engine="redb", quiet=False)
            with self.assertRaises(RuntimeError):
                MODULE.import_results(source, root / "v2", {"persy"})

    def test_refuses_initialized_destination(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = self.make_source(root)
            write_case(source, "ok", engine="redb")
            dest = root / "v2"
            dest.mkdir()
            (dest / "support.json").write_text("{}\n")
            with self.assertRaises(RuntimeError):
                MODULE.import_results(source, dest, {"persy"})


if __name__ == "__main__":
    unittest.main()
