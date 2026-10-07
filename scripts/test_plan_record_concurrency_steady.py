from __future__ import annotations
import importlib.util,hashlib,json,tempfile,unittest
from pathlib import Path
SCRIPT=Path(__file__).with_name("plan-record-concurrency-steady.py")
SPEC=importlib.util.spec_from_file_location("plan_record_concurrency_steady",SCRIPT); assert SPEC and SPEC.loader
MOD=importlib.util.module_from_spec(SPEC); SPEC.loader.exec_module(MOD)
class Tests(unittest.TestCase):
    def test_final_plan_is_three_trial_and_transaction_aligned(self):
        cases=[]
        for workload,state,txn in [("point-read","growth",100),("write-burst","bounded",100)]:
            for clients in (1,8):
                cases.append({"scenario":"record-concurrency-core","engine":"sqlite","durability":"sync","workload":workload,"clients":clients,"records":10000,"ops":1000,"payload_bytes":512,"txn_size":txn,"trial":1,"state_evolution":state})
        plan={"record_concurrency_plan_version":1,"kind":"semantic-calibration","expect_trials":1,"case_count":len(cases),"cases":cases}
        with tempfile.TemporaryDirectory() as td:
            t=Path(td); pp=t/"plan.json"; pp.write_text(json.dumps(plan)); run=t/"run"; (run/"cases").mkdir(parents=True)
            for i,item in enumerate(cases):
                rate=1000.0 if item["clients"]==1 else 100.0
                row=dict(item); row["ops_requested"]=row.pop("ops"); row["ops_completed"]=row["ops_requested"]; row["elapsed_s"]=row["ops_requested"]/rate; row["ops_per_s"]=rate
                (run/"cases"/f"{i}.json").write_text(json.dumps(row))
            sha=hashlib.sha256(pp.read_bytes()).hexdigest(); (run/"support.json").write_text(json.dumps({"plan_sha256":sha})); (run/"summary.json").write_text(json.dumps({"row_count":4,"group_count":4,"problems":[]}))
            out=MOD.build_final(pp,run,3)
            self.assertEqual(out["family_count"],2); self.assertEqual(out["case_count"],12); self.assertEqual(out["expect_trials"],3)
            for fam in out["families"]:
                self.assertLessEqual(fam["projected_slowest_s"],15.0)
                if fam["workload"]=="write-burst": self.assertEqual(fam["final_ops"]%100,0)
            for fam in {(x["scenario"],x["engine"],x["workload"]):x["ops"] for x in out["cases"]}.values(): self.assertGreater(fam,0)
            point=next(f for f in out["families"] if f["workload"]=="point-read")
            self.assertEqual(point["final_ops"],1500)
            self.assertTrue(point["reduced_from_calibration"] is False)

    def test_final_plan_can_reduce_overlong_calibration(self):
        cases=[]
        for clients in (1,8):
            cases.append({"scenario":"record-concurrency-core","engine":"sqlite","durability":"sync","workload":"point-read","clients":clients,"records":10000,"ops":10000,"payload_bytes":512,"txn_size":100,"trial":1,"state_evolution":"growth"})
        plan={"record_concurrency_plan_version":1,"kind":"semantic-calibration","expect_trials":1,"case_count":2,"cases":cases}
        with tempfile.TemporaryDirectory() as td:
            t=Path(td); pp=t/"plan.json"; pp.write_text(json.dumps(plan)); run=t/"run"; (run/"cases").mkdir(parents=True)
            for i,item in enumerate(cases):
                rate=1000.0 if item["clients"]==1 else 500.0
                row=dict(item); row["ops_requested"]=row.pop("ops"); row["ops_completed"]=row["ops_requested"]; row["elapsed_s"]=row["ops_requested"]/rate; row["ops_per_s"]=rate
                (run/"cases"/f"{i}.json").write_text(json.dumps(row))
            sha=hashlib.sha256(pp.read_bytes()).hexdigest(); (run/"support.json").write_text(json.dumps({"plan_sha256":sha})); (run/"summary.json").write_text(json.dumps({"row_count":2,"group_count":2,"problems":[]}))
            out=MOD.build_final(pp,run,3); fam=out["families"][0]
            self.assertEqual(fam["final_ops"],3000)
            self.assertTrue(fam["reduced_from_calibration"])
            self.assertAlmostEqual(fam["projected_fastest_s"],3.0)
            self.assertAlmostEqual(fam["projected_slowest_s"],6.0)
if __name__=="__main__": unittest.main()
