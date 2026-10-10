#!/usr/bin/env -S uv run --script
from __future__ import annotations

import argparse
from pathlib import Path

DEFAULT_RUNTIME = Path("/srv/scratch/db-bench-work/record-concurrency-steady-runtime")
READY = Path("/srv/scratch/db-bench-work/record-full-v1/ready.json")
BINARY_COMMIT = "01b8a5c4ecfd79e69f0c98cef823d7fe79401d25"
ADMISSION = "pre-io+pre/post-external-v2"

def dropin_text(runtime: Path = DEFAULT_RUNTIME) -> str:
    checker = runtime / "scripts/check-record-ready-certificate.py"
    return (
        "[Service]\n"
        f"ExecStartPre=/usr/bin/uv run --script {checker} {READY} "
        f"--expected-binary-commit {BINARY_COMMIT} "
        f"--expected-admission-policy {ADMISSION} "
        "--expected-groups 40 --expected-rows 200\n"
    )

def main() -> int:
    ap = argparse.ArgumentParser(description="Generate the record-concurrency readiness-v4 systemd drop-in")
    ap.add_argument("--runtime", type=Path, default=DEFAULT_RUNTIME)
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(dropin_text(args.runtime))
    print(args.out)
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
