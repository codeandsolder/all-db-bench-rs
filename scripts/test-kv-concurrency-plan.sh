#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bash -n "$ROOT/scripts/run-kv-concurrency-plan.sh"
python3 - <<'PY'
import json,tempfile,subprocess,pathlib
# Plan generation is covered by Python unit tests; this smoke verifies the real
# generated plan has unique client groups and fixed records/ops within family.
p=pathlib.Path('/srv/scratch/db-bench-work/concurrency-sizing-audit/20261007-kv-concurrency-missing-client-groups-v1.json')
if p.exists():
    plan=json.loads(p.read_text())
    assert plan['case_count']==len(plan['cases'])
    seen=set(); fam={}
    for c in plan['cases']:
        k=(c['scenario'],c['engine'],c['durability'],c['workload'],c['clients'])
        assert k not in seen; seen.add(k)
        fk=k[:-1]; pair=(c['records'],c['ops'])
        assert fam.setdefault(fk,pair)==pair
print('kv-concurrency-plan-ok')
PY
