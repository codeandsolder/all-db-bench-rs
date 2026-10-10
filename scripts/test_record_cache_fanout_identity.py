from __future__ import annotations
import importlib.util, json, sys, tempfile, unittest
from pathlib import Path
SCRIPT = Path(__file__).with_name("summarize.py")
SPEC = importlib.util.spec_from_file_location("summarize_cache_fanout", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC); sys.modules[SPEC.name]=M; SPEC.loader.exec_module(M)
def row():
    return {"format_version":8,"lane":"record","scenario":"indexed-cache-fanout","engine":"sqlite","engine_version":"rusqlite 0.40.2 / SQLite 3.53.4","durability":"sync","workload":"indexed-read","records":100000,"ops_requested":10000,"payload_bytes":512,"txn_size":100,"indexed_read_limit":100,"sql_cache_kib":2000,"read_materialization":"full-record-v1","write_materialization":"no-return-v1"}
class Tests(unittest.TestCase):
    def test_limit_is_identity(self):
        a=row(); b=dict(a,indexed_read_limit=10); self.assertNotEqual(M.group_key(a),M.group_key(b))
    def test_cache_budget_is_identity(self):
        a=row(); b=dict(a,sql_cache_kib=8000); self.assertNotEqual(M.group_key(a),M.group_key(b))
    def test_legacy_defaults_preserve_existing_rows(self):
        a=row(); a.pop("indexed_read_limit"); a.pop("sql_cache_kib"); b=dict(row(), indexed_read_limit=100, sql_cache_kib=None); self.assertEqual(M.group_key(a),M.group_key(b))
    def test_summary_main_formats_cache_budget(self):
        sample = row()
        sample.update({
            "trial": 1, "ops_completed": 10000, "ops_per_s": 1000.0,
            "operation_latency": {"count": 1, "p99_us": 1.0},
            "transaction_latency": {"count": 0, "p99_us": 0.0},
            "db_bytes": 1, "peak_rss_kib": 1, "open_s": 0.0, "prefill_s": 0.0,
            "elapsed_s": 10.0, "durability_mapping": "diagnostic",
            "measured_process": {"cpu_runtime_ns": 1, "cpu_runtime_fraction_of_wall": 1.0},
            "measured_system_delta": {},
        })
        with tempfile.TemporaryDirectory() as tmp:
            src=Path(tmp)/"in.ndjson"; out=Path(tmp)/"out.json"
            src.write_text(json.dumps(sample)+"\n")
            old=sys.argv; sys.argv=["summarize.py",str(src),"--json-out",str(out),"--markdown-out",str(Path(tmp)/"out.md")]
            try: M.main()
            finally: sys.argv=old
            summary=json.loads(out.read_text())
        self.assertIn("sql_cache_kib=2000", summary["groups"][0]["configuration"])
if __name__ == "__main__": unittest.main()
