#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
from pathlib import Path

AUDIT = Path("/srv/scratch/db-bench-work/kv-sizing-audit/20261006-kv-quick-67228ce-v4.json")
BASE_SELECTED = Path("/srv/scratch/db-bench-work/kv-sizing-audit/selected-final-v4")
PLAN = Path("/srv/scratch/db-bench-work/kv-sizing-audit/20261010-kv-baseline-admission-v2-repair-plan.json")
OUT_SELECTED = Path("/srv/scratch/db-bench-work/kv-sizing-audit/selected-final-v5")
OUT_COMPLETE = OUT_SELECTED / "complete.json"
BIN = Path("/srv/scratch/db-bench-work/kv-sizing-audit/bin/kvbench-v3-360c3b398babd4710")
BIN_SHA256 = "360c3b398babd4710a179cf102ad2cfe1fbd5e39fcf32a288fc771a75fa7f563"
RUN_PREFIX = "20261010-kv-admission-v2-repair"
STATUS = Path("/srv/scratch/db-bench-work/kv-sizing-audit/admission-v2-repair-idle-status.json")
LOCK = Path("/run/lock/all-db-bench-performance.lock")
RECORD_BASE = Path("/srv/scratch/db-bench-work/record-full-v1")
RECORD_RUNTIME = RECORD_BASE / "runtime"
RECORD_READY = RECORD_BASE / "ready.json"
RECORD_SELECTED = RECORD_BASE / "selected-final-v3"
RECORD_BINARY_MANIFEST = RECORD_BASE / "bin/manifest.json"
RECORD_BINARY_COMMIT = "01b8a5c4ecfd79e69f0c98cef823d7fe79401d25"
RECORD_STOCK_RUN = "20261010-record-full-v1-stock-v2"
ADMISSION = "pre-io+pre/post-external-v2"
KV_SERVICE = "all-db-bench-concurrency-sizing.service"
SERVICE = "all-db-bench-kv-baseline-admission-repair.service"


def unit_text(runtime: Path) -> str:
    ready_verifier = runtime / "scripts" / "verify-record-ready.py"
    planner = runtime / "scripts" / "plan-kv-baseline-admission-repairs.py"
    runner = runtime / "scripts" / "run-kv-sizing-followups.py"
    finalizer = runtime / "scripts" / "finalize-kv-baseline-admission-repairs.py"
    return f"""[Unit]
Description=all-db-bench repair legacy KV baseline admission-selected groups
After=local-fs.target all-db-bench-record-final-v3.service
ConditionPathExists={RECORD_READY}
ConditionPathExists={RECORD_SELECTED / 'manifest.json'}
ConditionPathExists={BASE_SELECTED / 'manifest.json'}
ConditionPathExists={BIN}
ConditionPathExists=!{OUT_COMPLETE}
OnSuccess={KV_SERVICE}

[Service]
Type=oneshot
WorkingDirectory={runtime}
ExecStartPre=/usr/bin/uv run --no-project python {ready_verifier} --selected {RECORD_SELECTED} --binary-manifest {RECORD_BINARY_MANIFEST} --expected-commit {RECORD_BINARY_COMMIT} --repo {RECORD_RUNTIME} --stock-run-id {RECORD_STOCK_RUN} --expected-admission-policy {ADMISSION} --out {RECORD_READY}
ExecStartPre=/usr/bin/uv run --no-project python {planner} --audit {AUDIT} --manifest {BASE_SELECTED / 'manifest.json'} --out {PLAN}
ExecStart=/usr/bin/uv run --no-project python {runner} --repo {runtime} --plan {AUDIT} --quality-plan {PLAN} --quality-only --run-prefix {RUN_PREFIX} --bench-bin {BIN} --expected-bench-sha256 {BIN_SHA256} --lock-file {LOCK} --status-file {STATUS} --watch --busy-sleep 1
ExecStart=/usr/bin/uv run --no-project python {finalizer} --base-selected-dir {BASE_SELECTED} --repair-plan {PLAN} --repair-repo {runtime} --run-prefix {RUN_PREFIX} --audit {AUDIT} --out-dir {OUT_SELECTED} --expected-admission-policy {ADMISSION} --expected-bench-sha256 {BIN_SHA256}
KillMode=control-group
TimeoutStopSec=15s
UMask=0022

[Install]
WantedBy=multi-user.target
"""


def record_handoff_dropin() -> str:
    # OnSuccess dependencies are additive in drop-ins. The KV unit therefore
    # also has a hard v5 condition; the legacy direct handoff is harmlessly
    # skipped until this repair succeeds.
    return f"""[Unit]
ConditionPathExists=!{RECORD_READY}
OnSuccess={SERVICE}
"""


def kv_v5_gate_dropin(runtime: Path) -> str:
    verifier = runtime / "scripts" / "check-kv-baseline-selection-ready.py"
    return f"""[Unit]
ConditionPathExists={OUT_COMPLETE}

[Service]
ExecStartPre=/usr/bin/uv run --no-project python {verifier} {OUT_SELECTED} --expected-rows 1046 --expected-groups 180 --expected-repairs 5
"""


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Generate the KV baseline admission-repair unit and handoff gates"
    )
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--unit-out", type=Path, required=True)
    parser.add_argument("--record-dropin-out", type=Path, required=True)
    parser.add_argument("--kv-gate-dropin-out", type=Path, required=True)
    args = parser.parse_args()
    for path in (args.unit_out, args.record_dropin_out, args.kv_gate_dropin_out):
        path.parent.mkdir(parents=True, exist_ok=True)
    args.unit_out.write_text(unit_text(args.runtime))
    args.record_dropin_out.write_text(record_handoff_dropin())
    args.kv_gate_dropin_out.write_text(kv_v5_gate_dropin(args.runtime))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
