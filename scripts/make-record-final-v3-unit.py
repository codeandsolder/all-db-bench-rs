#!/usr/bin/env -S uv run --script
from __future__ import annotations

import argparse
import json
from pathlib import Path

BASE = Path('/srv/scratch/db-bench-work/record-full-v1')
RUNTIME = BASE / 'runtime'
MANIFEST = BASE / 'bin/manifest.json'
BASE_PLAN = BASE / '20261010-record-final-v3-base-plan.json'
PLAN = BASE / '20261010-record-final-v3-plan.json'
PLAN2 = BASE / '20261010-record-final-v3-plan-r2.json'
STOCK_RESULTS = RUNTIME / 'results/runs/20261010-record-full-v1-stock-v2/results.ndjson'
SIZING_RESULTS = BASE / 'selected-final-v2/results.ndjson'
STOCK_AUDIT = BASE / '20261010-record-full-v1-stock-v2-sizing-v3.json'
EMPTY_SIZING = BASE / 'quality-only-empty-sizing-v3.json'
SELECTED = BASE / 'selected-final-v3'
READY = BASE / 'ready.json'
RUN_PREFIX = '20261010-record-final-v3'
LOCK = '/run/lock/all-db-bench-performance.lock'
ADMISSION = 'pre-io+pre/post-external-v2'
BINARY_COMMIT = '01b8a5c4ecfd79e69f0c98cef823d7fe79401d25'


def load_manifest(path: Path) -> dict:
    m = json.loads(path.read_text())
    if m.get('repo_commit') != BINARY_COMMIT:
        raise ValueError(f"unexpected binary commit: {m.get('repo_commit')!r}")
    if m.get('read_materialization') != 'full-record-v1' or m.get('write_materialization') != 'no-return-v1':
        raise ValueError('record binary semantic mismatch')
    required = {
        'recordbench',
        'surrealdb-rocksdb-recordbench',
    }
    missing = required - set(m.get('binaries', {}))
    if missing:
        raise ValueError(f'missing record binaries: {sorted(missing)}')
    return m


def unit_text(m: dict) -> str:
    rb = m['binaries']['recordbench']
    rocks = m['binaries']['surrealdb-rocksdb-recordbench']
    return f'''[Unit]
Description=all-db-bench record final v3 five-trial confirmation
After=local-fs.target
ConditionPathExists={MANIFEST}
ConditionPathExists={SIZING_RESULTS}
OnSuccess=all-db-bench-concurrency-sizing.service

[Service]
Type=oneshot
WorkingDirectory={RUNTIME}
ExecStartPre=/usr/bin/uv run --script {RUNTIME}/scripts/make-record-final-confirmation-plan.py {SIZING_RESULTS} --out {BASE_PLAN}
ExecStart=/usr/bin/uv run --script {RUNTIME}/scripts/run-record-sizing-followups.py --repo {RUNTIME} --plan {EMPTY_SIZING} --quality-plan {BASE_PLAN} --bench-bin {rb['path']} --rocks-bench-bin {rocks['path']} --expected-bench-sha256 {rb['sha256']} --expected-rocks-bench-sha256 {rocks['sha256']} --lock-file {LOCK} --status-file {BASE}/final-v3-base-idle-status.json --run-prefix {RUN_PREFIX} --watch --busy-sleep 1
ExecStart=/usr/bin/uv run --script {RUNTIME}/scripts/refine-record-final-confirmation-plan.py {BASE_PLAN} --run-root {RUNTIME}/results/runs --thresholds-from-audit {STOCK_AUDIT} --run-prefix {RUN_PREFIX} --out {PLAN}
ExecStart=/usr/bin/uv run --script {RUNTIME}/scripts/run-record-sizing-followups.py --repo {RUNTIME} --plan {EMPTY_SIZING} --quality-plan {PLAN} --bench-bin {rb['path']} --rocks-bench-bin {rocks['path']} --expected-bench-sha256 {rb['sha256']} --expected-rocks-bench-sha256 {rocks['sha256']} --lock-file {LOCK} --status-file {BASE}/final-v3-idle-status.json --run-prefix {RUN_PREFIX} --watch --busy-sleep 1
ExecStart=/usr/bin/uv run --script {RUNTIME}/scripts/refine-record-final-confirmation-plan.py {PLAN} --run-root {RUNTIME}/results/runs --thresholds-from-audit {STOCK_AUDIT} --run-prefix {RUN_PREFIX} --out {PLAN2}
ExecStart=/usr/bin/uv run --script {RUNTIME}/scripts/run-record-sizing-followups.py --repo {RUNTIME} --plan {EMPTY_SIZING} --quality-plan {PLAN2} --bench-bin {rb['path']} --rocks-bench-bin {rocks['path']} --expected-bench-sha256 {rb['sha256']} --expected-rocks-bench-sha256 {rocks['sha256']} --lock-file {LOCK} --status-file {BASE}/final-v3-r2-idle-status.json --run-prefix {RUN_PREFIX} --watch --busy-sleep 1
ExecStart=/usr/bin/uv run --script {RUNTIME}/scripts/finalize-baseline-sizing.py --repo {RUNTIME} --record-only --record-stock-results {STOCK_RESULTS} --record-audit {STOCK_AUDIT} --record-resize-prefix {RUN_PREFIX} --record-out-dir {SELECTED} --record-quality-repair-plan {PLAN2}
ExecStart=/usr/bin/uv run --no-project python {RUNTIME}/scripts/verify-record-ready.py --selected {SELECTED} --binary-manifest {MANIFEST} --expected-commit {BINARY_COMMIT} --repo {RUNTIME} --stock-run-id 20261010-record-full-v1-stock-v2 --expected-admission-policy {ADMISSION} --out {READY}
KillMode=control-group
TimeoutStopSec=15s
UMask=0022

[Install]
WantedBy=multi-user.target
'''


def main() -> int:
    parser = argparse.ArgumentParser(description='Generate the record final-v3 systemd unit')
    parser.add_argument('--manifest', type=Path, default=MANIFEST)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    text = unit_text(load_manifest(args.manifest))
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(text)
    print(args.out)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
