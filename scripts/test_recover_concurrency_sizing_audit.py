from __future__ import annotations
import importlib.util, json, tempfile, unittest
from pathlib import Path
SCRIPT=Path(__file__).with_name("recover-concurrency-sizing-audit.py")
SPEC=importlib.util.spec_from_file_location("recover_concurrency_sizing_audit",SCRIPT); assert SPEC and SPEC.loader
MOD=importlib.util.module_from_spec(SPEC); SPEC.loader.exec_module(MOD)
class Tests(unittest.TestCase):
    def test_recovers(self):
        cached={"concurrency_sizing_policy_version":1,"run_dir":"/old","additional_run_dirs":[],"source_support":{"trials":3},"thresholds":{"floor_seconds":2.0,"target_seconds":3.0,"severe_seconds":0.25,"stateful_max_trials":15,"safe_more_ops_workloads":["point-read","range-scan","read-heavy"],"state_changing_workloads":["balanced","tiny-txn","write-burst","churn","delete-burst"]},"row_count":1,"planned_case_count":6,"planned_client_group_count":2,"observed_client_group_count":1,"planned_family_count":1,"observed_family_count":1,"missing_probe_case_count":1,"counts":{"needs-probe":1},"client_groups":[{"scenario":"concurrency-primary","engine":"redb","durability":"sync","workload":"point-read","clients":1,"records":100,"ops":1000,"observed_trials":[1],"observed_count":1,"median_elapsed_s":1.0,"median_ops_per_s":1000.0,"min_elapsed_s":1.0,"max_elapsed_s":1.0},{"scenario":"concurrency-primary","engine":"redb","durability":"sync","workload":"point-read","clients":8,"records":100,"ops":1000,"observed_trials":[],"observed_count":0}],"families":[{"scenario":"concurrency-primary","engine":"redb","durability":"sync","workload":"point-read","clients":[1,8],"missing_clients":[8],"current_records":100,"current_ops":1000,"observed_client_groups":1,"planned_client_groups":2,"semantics":"fixed-keyspace","status":"needs-probe","recommendation":"collect-stock-probe"}],"missing_probe_cases":[{"scenario":"concurrency-primary","engine":"redb","durability":"sync","workload":"point-read","clients":8,"records":100,"ops":1000,"trial":1}]}
        with tempfile.TemporaryDirectory() as td:
            t=Path(td); cp=t/"cached.json"; cp.write_text(json.dumps(cached)); run=t/"run"; (run/"cases").mkdir(parents=True)
            row={"scenario":"concurrency-primary","engine":"redb","durability":"sync","workload":"point-read","clients":8,"records":100,"ops_requested":1000,"ops_completed":1000,"trial":1,"elapsed_s":2.0,"ops_per_s":500.0}
            (run/"cases"/"case.json").write_text(json.dumps(row)); (run/"support.json").write_text(json.dumps({"case_count":1})); (run/"summary.json").write_text(json.dumps({"row_count":1,"group_count":1,"problems":[]}))
            r=MOD.recover(cp,run); self.assertEqual(r["row_count"],2); self.assertEqual(r["observed_client_group_count"],2); self.assertEqual(r["missing_probe_case_count"],0); f=r["families"][0]; self.assertEqual(f["status"],"undersized"); self.assertEqual(f["suggested_ops"],5000)
if __name__=="__main__": unittest.main()
