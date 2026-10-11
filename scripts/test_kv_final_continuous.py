from __future__ import annotations
import importlib.util, json, tempfile, unittest
from pathlib import Path

def load(name:str):
    path=Path(__file__).with_name(name); spec=importlib.util.spec_from_file_location(name.replace('-','_'),path); assert spec and spec.loader; m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m); return m
PLAN=load('make-kv-final-continuous-plan.py'); UNIT=load('make-kv-final-continuous-unit.py'); FINAL=load('finalize-kv-final-continuous.py')

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

    @staticmethod
    def _support() -> dict:
        h = "a" * 64
        return {
            "lane": "kv", "profile": "quick",
            "admission_policy": FINAL.ADMISSION,
            "benchmark_binary_sha256": "b" * 64,
            "runner_sha256": h, "kv_matrix_policy_sha256": "c" * 64,
            "performance_common_sha256": "1" * 64, "continuous_noise_common_sha256": "2" * 64,
            "noise_guard_sha256": "d" * 64, "continuous_noise_guard_sha256": "e" * 64,
            "continuous_noise_sample_ms": 250, "continuous_noise_max_cpu_percent": 50,
            "continuous_noise_max_io_average_mib_s": 2, "continuous_noise_max_io_rate_mib_s": 8,
            "build_profile": "external", "hostname": "sf314-42",
            "machine_id_sha256": "f" * 64, "filesystem": "zfs", "source": "rpool/srv/scratch",
            "initial_min_free_gib": 10, "case_min_free_gib": "10",
        }

    def test_support_provenance_binds_runner_host_and_storage_identity(self):
        support = self._support()
        identity = FINAL.support_provenance(support, "run-a", "b" * 64)
        self.assertEqual(identity["runner_sha256"], "a" * 64)
        self.assertEqual(identity["hostname"], "sf314-42")
        self.assertEqual(identity["source"], "rpool/srv/scratch")
        other = dict(identity); other["hostname"] = "other-host"
        encoded = {json.dumps(identity, sort_keys=True, separators=(",",":")), json.dumps(other, sort_keys=True, separators=(",",":"))}
        self.assertEqual(len(encoded), 2, "host changes must make campaign provenance heterogeneous")

    def test_support_provenance_rejects_malformed_runner_sha(self):
        support = self._support(); support["runner_sha256"] = "not-a-sha"
        with self.assertRaisesRegex(ValueError, "malformed runner_sha256"):
            FINAL.support_provenance(support, "run-a", "b" * 64)

    def test_support_provenance_rejects_wrong_policy_and_floor(self):
        support = self._support(); support["admission_policy"] = "old"
        with self.assertRaisesRegex(ValueError, "wrong admission"):
            FINAL.support_provenance(support, "run-a", "b" * 64)
        support = self._support(); support["case_min_free_gib"] = 0
        with self.assertRaisesRegex(ValueError, "KV v6 free-space floor below 10 GiB"):
            FINAL.support_provenance(support, "run-a", "b" * 64)
        support = self._support(); support["imported_from_run"] = "historical-run"
        with self.assertRaisesRegex(ValueError, "imported cases are forbidden"):
            FINAL.support_provenance(support, "run-a", "b" * 64)

    def test_unit_replaces_handoff_and_gate(self):
        u=UNIT.unit_text(); self.assertIn('ConditionPathExists=!/srv/scratch/db-bench-work/kv-sizing-audit/ready-v6.json',u); self.assertIn('OnSuccess=all-db-bench-concurrency-sizing.service',u); self.assertIn('--quality-only',u)
        self.assertIn('OnSuccess=\nOnSuccess=all-db-bench-kv-final-continuous.service',UNIT.record_handoff_dropin())
        g=UNIT.concurrency_gate_dropin(); self.assertIn('ConditionPathExists=\nConditionPathExists=/srv/scratch/db-bench-work/kv-sizing-audit/ready-v6.json',g); self.assertIn('ExecStartPre=\nExecStartPre=/usr/bin/uv run --script',g)
if __name__=='__main__': unittest.main()
