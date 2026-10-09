#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import hashlib
import subprocess
import sys
from pathlib import Path

KV_SHA = "360c3b398babd4710a179cf102ad2cfe1fbd5e39fcf32a288fc771a75fa7f563"
KV_BIN = Path("/srv/scratch/db-bench-work/kv-sizing-audit/bin/kvbench-v3-360c3b398babd4710")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_pinned_binaries() -> None:
    for path, expected in ((KV_BIN, KV_SHA),):
        actual = sha256(path)
        if actual != expected:
            raise RuntimeError(f"pinned benchmark SHA-256 mismatch: {path}: expected {expected}, got {actual}")


def run_stage(command: list[str], *, cwd: Path) -> int:
    print(f"queue stage: {Path(command[1]).name}", flush=True)
    return subprocess.run(command, cwd=cwd, check=False).returncode


def main() -> int:
    parser = argparse.ArgumentParser(description="Run DB performance campaigns serially whenever the laptop is idle")
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--busy-sleep", type=float, default=2.0)
    args = parser.parse_args()

    repo = args.repo
    # Verify large immutable binaries before waiting for a quiet performance
    # window. Stage supervisors intentionally receive no checksum argument so
    # they cannot self-contaminate a newly admitted window with large reads.
    verify_pinned_binaries()
    print("pinned benchmark binaries verified", flush=True)

    stages = [
        [
            sys.executable,
            str(repo / "scripts" / "run-kv-sizing-followups.py"),
            "--repo",
            str(repo),
            "--plan",
            "/srv/scratch/db-bench-work/kv-sizing-audit/20261006-kv-quick-67228ce-v4.json",
            "--quality-plan",
            "/srv/scratch/db-bench-work/kv-sizing-audit/20261006-kv-stock-pressure-repairs-refined-v1.json",
            "--bench-bin",
            "/srv/scratch/db-bench-work/kv-sizing-audit/bin/kvbench-v3-360c3b398babd4710",
            "--lock-file",
            "/run/lock/all-db-bench-performance.lock",
            "--status-file",
            "/srv/scratch/db-bench-work/kv-sizing-audit/idle-status.json",
            "--watch",
            "--busy-sleep",
            str(args.busy_sleep),
            "--calibration-count",
            "5",
            "--calibration-min-seconds",
            "1.5",
            "--calibration-max-seconds",
            "15.0",
        ],
    ]
    for stage in stages:
        rc = run_stage(stage, cwd=repo)
        if rc != 0:
            print(f"queue stopped: stage={Path(stage[1]).name} rc={rc}", flush=True)
            return rc
    print("legacy DB idle queue complete (KV-only; corrected record campaigns use dedicated services)", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
