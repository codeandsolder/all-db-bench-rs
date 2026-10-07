from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("finalize-concurrency-campaign.py")
SPEC = importlib.util.spec_from_file_location("finalize_concurrency_campaign", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class FinalizeConcurrencyCampaignTests(unittest.TestCase):
    def test_validate_complete(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            (run / "support.json").write_text(json.dumps({"case_count": 12, "trials": 3}))
            (run / "summary.json").write_text(json.dumps({"row_count": 12, "group_count": 4, "problems": []}))
            MODULE.validate_complete(run)

    def test_validate_rejects_problem_or_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            (run / "support.json").write_text(json.dumps({"case_count": 12, "trials": 3}))
            (run / "summary.json").write_text(json.dumps({"row_count": 12, "group_count": 4, "problems": ["bad"]}))
            with self.assertRaises(RuntimeError): MODULE.validate_complete(run)
            (run / "summary.json").write_text(json.dumps({"row_count": 12, "group_count": 4, "problems": []}))
            (run / "failures.ndjson").write_text("{}\n")
            with self.assertRaises(RuntimeError): MODULE.validate_complete(run)


if __name__ == "__main__":
    unittest.main()
