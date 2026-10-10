#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
from pathlib import Path

BASE = Path("/srv/scratch/db-bench-work/record-full-v1")
HISTORICAL_RUNTIME = BASE / "runtime"
RUNTIME = Path("/srv/scratch/db-bench-work/record-final-continuous-runtime")
MANIFEST = BASE / "bin/manifest.json"
PLAN = BASE / "20261010-record-final-v3-plan-r2.json"
STOCK_RESULTS = HISTORICAL_RUNTIME / "results/runs/20261010-record-full-v1-stock-v2/results.ndjson"
STOCK_AUDIT = BASE / "20261010-record-full-v1-stock-v2-sizing-v3.json"
EMPTY_SIZING = BASE / "quality-only-empty-sizing-v3.json"
HISTORICAL_SELECTED = BASE / "selected-final-v3"
HISTORICAL_READY = BASE / "ready.json"
SELECTED = BASE / "selected-final-v4"
READY = BASE / "ready-v4.json"
STATUS = BASE / "final-v4-continuous-idle-status.json"
RUN_PREFIX = "20261010-record-final-v4-continuous"
LOCK = "/run/lock/all-db-bench-performance.lock"
ADMISSION = "pre-io+pre/continuous/post-external-v3"
BINARY_COMMIT = "01b8a5c4ecfd79e69f0c98cef823d7fe79401d25"
REPAIR_SERVICE = "all-db-bench-kv-baseline-admission-repair.service"


def load_manifest(path: Path) -> dict:
    manifest = json.loads(path.read_text())
    if manifest.get("repo_commit") != BINARY_COMMIT:
        raise ValueError(f"unexpected binary commit: {manifest.get('repo_commit')!r}")
    if (
        manifest.get("read_materialization") != "full-record-v1"
        or manifest.get("write_materialization") != "no-return-v1"
    ):
        raise ValueError("record binary semantic mismatch")
    required = {"recordbench", "surrealdb-rocksdb-recordbench"}
    missing = required - set(manifest.get("binaries", {}))
    if missing:
        raise ValueError(f"missing record binaries: {sorted(missing)}")
    return manifest


def unit_text(manifest: dict, runtime: Path = RUNTIME) -> str:
    recordbench = manifest["binaries"]["recordbench"]
    rocks = manifest["binaries"]["surrealdb-rocksdb-recordbench"]
    return f"""[Unit]
Description=all-db-bench record final v4 continuous-admission confirmation
After=local-fs.target all-db-bench-record-final-v3.service
ConditionPathExists={HISTORICAL_READY}
ConditionPathExists={HISTORICAL_SELECTED / 'manifest.json'}
ConditionPathExists={PLAN}
ConditionPathExists={MANIFEST}
ConditionPathExists=!{READY}
OnSuccess={REPAIR_SERVICE}

[Service]
Type=oneshot
WorkingDirectory={runtime}
ExecStart=/usr/bin/uv run --script {runtime}/scripts/run-record-sizing-followups.py --repo {runtime} --plan {EMPTY_SIZING} --quality-plan {PLAN} --bench-bin {recordbench['path']} --rocks-bench-bin {rocks['path']} --expected-bench-sha256 {recordbench['sha256']} --expected-rocks-bench-sha256 {rocks['sha256']} --lock-file {LOCK} --status-file {STATUS} --run-prefix {RUN_PREFIX} --watch --busy-sleep 1
ExecStart=/usr/bin/uv run --script {runtime}/scripts/finalize-baseline-sizing.py --repo {runtime} --record-only --record-stock-results {STOCK_RESULTS} --record-audit {STOCK_AUDIT} --record-resize-prefix {RUN_PREFIX} --record-out-dir {SELECTED} --record-quality-repair-plan {PLAN}
ExecStart=/usr/bin/uv run --no-project python {runtime}/scripts/verify-record-ready.py --selected {SELECTED} --binary-manifest {MANIFEST} --expected-commit {BINARY_COMMIT} --repo {runtime} --stock-run-id 20261010-record-full-v1-stock-v2 --expected-admission-policy {ADMISSION} --out {READY}
KillMode=control-group
TimeoutStopSec=15s
UMask=0022

[Install]
WantedBy=multi-user.target
"""


def v2_handoff_dropin() -> str:
    return f"""[Unit]
ConditionPathExists=!{HISTORICAL_READY}
OnSuccess=all-db-bench-record-final-continuous.service
"""


def record_ready_v5_dropin(runtime: Path) -> str:
    checker = runtime / "scripts/check-record-ready-certificate.py"
    return f"""[Unit]
ConditionPathExists={READY}

[Service]
ExecStartPre=/usr/bin/uv run --script {checker} {READY} --expected-binary-commit {BINARY_COMMIT} --expected-admission-policy {ADMISSION} --expected-groups 40 --expected-rows 200
"""


def sustained_ready_v5_dropin() -> str:
    return f"""[Unit]
ConditionPathExists={READY}
"""


def main() -> int:
    parser = argparse.ArgumentParser(description="Generate record continuous confirmation and v5 readiness gates")
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    parser.add_argument("--runtime", type=Path, default=RUNTIME)
    parser.add_argument("--unit-out", type=Path, required=True)
    parser.add_argument("--v2-handoff-out", type=Path, required=True)
    parser.add_argument("--record-calibration-dropin-out", type=Path, required=True)
    parser.add_argument("--record-final-dropin-out", type=Path, required=True)
    parser.add_argument("--sustained-dropin-out", type=Path, required=True)
    args = parser.parse_args()
    manifest = load_manifest(args.manifest)
    outputs = {
        args.unit_out: unit_text(manifest, args.runtime),
        args.v2_handoff_out: v2_handoff_dropin(),
        args.record_calibration_dropin_out: record_ready_v5_dropin(
            Path("/srv/scratch/db-bench-work/record-concurrency-steady-runtime")
        ),
        args.record_final_dropin_out: record_ready_v5_dropin(
            Path("/srv/scratch/db-bench-work/record-concurrency-steady-runtime")
        ),
        args.sustained_dropin_out: sustained_ready_v5_dropin(),
    }
    for path, text in outputs.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
