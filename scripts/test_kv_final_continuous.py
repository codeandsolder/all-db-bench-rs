from __future__ import annotations
import importlib.util, json, tempfile, unittest
from pathlib import Path

def load(name:str):
    path=Path(__file__).with_name(name); spec=importlib.util.spec_from_file_location(name.replace('-','_'),path); assert spec and spec.loader; m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m); return m
PLAN=load('make-kv-final-continuous-plan.py'); UNIT=load('make-kv-final-continuous-unit.py')

class KvFinalContinuousTest(unittest.TestCase):
    def test_plan_covers_all_groups_and_inverts_tiny_txn(self):
        with tempfile.TemporaryDirectory() as raw:
            d=Path(raw); rows=[]; sources=[]
            for i in range(180):
                engine='lsmdb' if i==0 else f'e{i}'; workload='range-scan' if i==0 else ('tiny-txn' if i==1 else 'balanced'); ops=1000 if i==0 else (5000 if i==1 else 50000); trials=3 if i%2==0 else 7
                for t in range(1,trials+1): rows.append({'engine':engine,'durability':'sync','workload':workload,'records':100000,'ops_requested':ops,'trial':t,'elapsed_s':1.0,'ops_per_s':float(ops)})
                sources.append({'engine':engine,'durability':'sync','workload':workload,'ops_requested':ops,'trials':trials,'median_elapsed_s':1.0,'final_status':'accepted'})
            (d/'results.ndjson').write_text(''.join(json.dumps(r)+'\n' for r in rows)); (d/'manifest.json').write_text(json.dumps({'complete':True,'selected_groups':180,'final_status_counts':{'undersized':0},'sources':sources})); (d/'summary.json').write_text(json.dumps({'group_count':180,'row_count':len(rows),'problems':[]}))
            plan=PLAN.make_plan(d,min_trials=5)
            self.assertEqual(plan['group_count'],180); self.assertEqual(len(plan['groups']),180); self.assertGreaterEqual(min(g['suggested_trials'] for g in plan['groups']),5)
            tiny=next(g for g in plan['groups'] if g['workload']=='tiny-txn'); self.assertEqual(tiny['suggested_effective_ops'],5000); self.assertEqual(tiny['runner_ops_override'],50000)
            lsm=next(g for g in plan['groups'] if g['engine']=='lsmdb'); self.assertEqual(lsm['runner_ops_override'],1000)
    def test_unit_replaces_handoff_and_gate(self):
        u=UNIT.unit_text(); self.assertIn('ConditionPathExists=!/srv/scratch/db-bench-work/kv-sizing-audit/ready-v6.json',u); self.assertIn('OnSuccess=all-db-bench-concurrency-sizing.service',u); self.assertIn('--quality-only',u)
        self.assertIn('OnSuccess=\nOnSuccess=all-db-bench-kv-final-continuous.service',UNIT.record_handoff_dropin())
        g=UNIT.concurrency_gate_dropin(); self.assertIn('ConditionPathExists=\nConditionPathExists=/srv/scratch/db-bench-work/kv-sizing-audit/ready-v6.json',g); self.assertIn('ExecStartPre=\nExecStartPre=/usr/bin/uv run --script',g)
if __name__=='__main__': unittest.main()
