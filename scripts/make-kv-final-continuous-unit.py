#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations
import argparse
from pathlib import Path
BASE=Path("/srv/scratch/db-bench-work/kv-sizing-audit"); RUNTIME=Path("/srv/scratch/db-bench-work/kv-final-continuous-runtime"); SOURCE=BASE/"selected-final-v4"; AUDIT=BASE/"20261006-kv-quick-67228ce-v4.json"; PLAN=BASE/"20261011-kv-final-v6-continuous-plan.json"; SELECTED=BASE/"selected-final-v6"; READY=BASE/"ready-v6.json"; STATUS=BASE/"final-v6-continuous-idle-status.json"; BIN=BASE/"bin/kvbench-v3-360c3b398babd4710"; BIN_SHA="360c3b398babd4710a179cf102ad2cfe1fbd5e39fcf32a288fc771a75fa7f563"; PREFIX="20261011-kv-final-v6-continuous"; LOCK=Path("/run/lock/all-db-bench-performance.lock"); RECORD_READY=Path("/srv/scratch/db-bench-work/record-full-v1/ready-v4.json"); SERVICE="all-db-bench-kv-final-continuous.service"; NEXT="all-db-bench-concurrency-sizing.service"

def unit_text(runtime:Path=RUNTIME)->str:
    return f'''[Unit]
Description=all-db-bench raw-KV final v6 continuous-admission confirmation
After=local-fs.target all-db-bench-record-final-continuous.service
ConditionPathExists={RECORD_READY}
ConditionPathExists={SOURCE/'results.ndjson'}
ConditionPathExists={SOURCE/'manifest.json'}
ConditionPathExists={SOURCE/'summary.json'}
ConditionPathExists={AUDIT}
ConditionPathExists={BIN}
ConditionPathExists=!{READY}
OnSuccess={NEXT}

[Service]
Type=oneshot
WorkingDirectory={runtime}
ExecStartPre=/usr/bin/uv run --script {runtime}/scripts/make-kv-final-continuous-plan.py {SOURCE} --min-trials 5 --out {PLAN}
ExecStart=/usr/bin/uv run --script {runtime}/scripts/run-kv-sizing-followups.py --repo {runtime} --plan {AUDIT} --quality-plan {PLAN} --quality-only --run-prefix {PREFIX} --bench-bin {BIN} --expected-bench-sha256 {BIN_SHA} --lock-file {LOCK} --status-file {STATUS} --watch --busy-sleep 1
ExecStart=/usr/bin/uv run --script {runtime}/scripts/finalize-kv-final-continuous.py --source-selected {SOURCE} --audit {AUDIT} --plan {PLAN} --repo {runtime} --run-prefix {PREFIX} --out-dir {SELECTED} --expected-bench-sha256 {BIN_SHA}
ExecStart=/usr/bin/uv run --script {runtime}/scripts/verify-kv-final-continuous.py --selected {SELECTED} --plan {PLAN} --repo {runtime} --expected-bench-sha256 {BIN_SHA} --out {READY}
KillMode=control-group
TimeoutStopSec=15s
UMask=0022

[Install]
WantedBy=multi-user.target
'''

def record_handoff_dropin()->str: return f'''[Unit]\nOnSuccess=\nOnSuccess={SERVICE}\n'''
def concurrency_gate_dropin(runtime:Path=RUNTIME)->str: return f'''[Unit]\nConditionPathExists=\nConditionPathExists={READY}\n\n[Service]\nExecStartPre=\nExecStartPre=/usr/bin/uv run --script {runtime}/scripts/verify-kv-final-continuous.py --selected {SELECTED} --plan {PLAN} --repo {runtime} --expected-bench-sha256 {BIN_SHA}\n'''

def main()->int:
    p=argparse.ArgumentParser(); p.add_argument("--runtime",type=Path,default=RUNTIME); p.add_argument("--unit-out",type=Path,required=True); p.add_argument("--record-handoff-out",type=Path,required=True); p.add_argument("--concurrency-gate-out",type=Path,required=True); a=p.parse_args()
    for path,text in ((a.unit_out,unit_text(a.runtime)),(a.record_handoff_out,record_handoff_dropin()),(a.concurrency_gate_out,concurrency_gate_dropin(a.runtime))): path.parent.mkdir(parents=True,exist_ok=True); path.write_text(text)
    return 0
if __name__=="__main__": raise SystemExit(main())
