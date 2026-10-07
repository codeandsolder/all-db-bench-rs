from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("validate-concurrency-plan.py")
SPEC = importlib.util.spec_from_file_location("validate_concurrency_plan", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC); sys.modules[SPEC.name] = M; SPEC.loader.exec_module(M)


def case(clients: int, trial: int, **extra):
    row = {"scenario":"concurrency-primary","engine":"redb","durability":"relaxed","workload":"balanced","clients":clients,"records":100000,"ops":50000,"trial":trial}
    row.update(extra); return row


class ValidateConcurrencyPlanTests(unittest.TestCase):
    def test_v1_defaults_to_growth_append(self) -> None:
        rows, meta = M.materialize({"concurrency_probe_plan_version":1,"case_count":1,"cases":[case(1,1)]})
        self.assertEqual(meta, {"plan_version":1,"expect_trials":1,"case_count":1})
        self.assertEqual(rows[0][-3:], ["growth","append","0"])

    def test_v2_multitrial_bounded_family(self) -> None:
        cases=[case(c,t,state_evolution="bounded",write_pattern="update-uniform",bounded_churn_slots=2400) for c in (1,8) for t in (1,2,3)]
        rows, meta=M.materialize({"concurrency_probe_plan_version":2,"expect_trials":3,"case_count":len(cases),"cases":cases})
        self.assertEqual(len(rows),6); self.assertEqual(meta["expect_trials"],3)

    def test_missing_trial_rejected(self) -> None:
        cases=[case(1,t,state_evolution="growth",write_pattern="append",bounded_churn_slots=0) for t in (1,2)]
        with self.assertRaisesRegex(ValueError,"expected 3"):
            M.materialize({"concurrency_probe_plan_version":2,"expect_trials":3,"case_count":2,"cases":cases})

    def test_bounded_append_rejected(self) -> None:
        bad=case(1,1); bad["workload"]="write-burst"; bad.update(state_evolution="bounded",write_pattern="append",bounded_churn_slots=0)
        with self.assertRaisesRegex(ValueError,"requires update"):
            M.materialize({"concurrency_probe_plan_version":2,"case_count":1,"cases":[bad]})

    def test_family_total_work_must_match_clients(self) -> None:
        a=case(1,1,state_evolution="growth",write_pattern="append",bounded_churn_slots=0)
        b=case(8,1,state_evolution="growth",write_pattern="append",bounded_churn_slots=0); b["ops"]=60000
        with self.assertRaisesRegex(ValueError,"family total-work mismatch"):
            M.materialize({"concurrency_probe_plan_version":2,"case_count":2,"cases":[a,b]})


if __name__ == "__main__": unittest.main()
