#!/usr/bin/env -S uv run --script
from __future__ import annotations

import argparse
import json
import re
from pathlib import Path

HEX64 = re.compile(r"^[0-9a-f]{64}$")
HEX40 = re.compile(r"^[0-9a-f]{40}$")
CONTINUOUS_ADMISSION_POLICY = "pre-io+pre/continuous/post-external-v3"


def validate_certificate(
    ready: dict,
    *,
    expected_binary_commit: str,
    expected_admission_policy: str,
    expected_groups: int = 40,
    expected_rows: int = 200,
) -> None:
    required_version = 5 if expected_admission_policy == CONTINUOUS_ADMISSION_POLICY else 4
    if int(ready.get("ready_version", -1)) < required_version:
        raise ValueError(
            f"readiness version is too old for admission policy: {ready.get('ready_version')!r} < {required_version}"
        )
    binary_commit = ready.get("binary_repo_commit")
    if binary_commit != expected_binary_commit or ready.get("repo_commit") != expected_binary_commit:
        raise ValueError("readiness binary commit mismatch")
    runner_commit = str(ready.get("runner_repo_commit", ""))
    if not HEX40.fullmatch(runner_commit):
        raise ValueError("readiness runner commit is missing or malformed")
    if ready.get("admission_policy") != expected_admission_policy:
        raise ValueError("readiness admission policy mismatch")
    if ready.get("read_materialization") != "full-record-v1":
        raise ValueError("readiness read semantics mismatch")
    if ready.get("write_materialization") != "no-return-v1":
        raise ValueError("readiness write semantics mismatch")
    if int(ready.get("group_count", -1)) != expected_groups:
        raise ValueError("readiness group count mismatch")
    if int(ready.get("row_count", -1)) != expected_rows:
        raise ValueError("readiness row count mismatch")
    for field in (
        "selected_noise_guard_sha256",
        "results_sha256",
        "selection_manifest_sha256",
        "summary_sha256",
        "binary_manifest_sha256",
    ):
        if not HEX64.fullmatch(str(ready.get(field, ""))):
            raise ValueError(f"readiness {field} is missing or malformed")

    if expected_admission_policy == CONTINUOUS_ADMISSION_POLICY:
        if not HEX64.fullmatch(str(ready.get("selected_continuous_noise_guard_sha256", ""))):
            raise ValueError("readiness continuous noise guard is missing or malformed")
        for field in (
            "continuous_noise_sample_ms",
            "continuous_noise_max_cpu_percent",
            "continuous_noise_max_io_bytes",
            "continuous_noise_max_io_rate_mib_s",
        ):
            value = ready.get(field)
            if not isinstance(value, (int, float)) or isinstance(value, bool) or value < 0:
                raise ValueError(f"readiness {field} is missing or invalid")
        if float(ready["continuous_noise_sample_ms"]) <= 0:
            raise ValueError("readiness continuous noise sample interval must be positive")

    supports = ready.get("source_support_sha256")
    if not isinstance(supports, dict) or len(supports) < expected_groups:
        raise ValueError("readiness source support provenance is incomplete")
    bad_supports = [
        name
        for name, digest in supports.items()
        if not name or not HEX64.fullmatch(str(digest))
    ]
    if bad_supports:
        raise ValueError(f"readiness source support hashes are malformed: {bad_supports[:3]}")


def main() -> int:
    ap = argparse.ArgumentParser(description="Fail closed unless a corrected record readiness certificate is acceptable")
    ap.add_argument("ready", type=Path)
    ap.add_argument("--expected-binary-commit", required=True)
    ap.add_argument("--expected-admission-policy", required=True)
    ap.add_argument("--expected-groups", type=int, default=40)
    ap.add_argument("--expected-rows", type=int, default=200)
    args = ap.parse_args()
    if not args.ready.is_file():
        raise SystemExit(f"record readiness certificate is missing: {args.ready}")
    ready = json.loads(args.ready.read_text())
    try:
        validate_certificate(
            ready,
            expected_binary_commit=args.expected_binary_commit,
            expected_admission_policy=args.expected_admission_policy,
            expected_groups=args.expected_groups,
            expected_rows=args.expected_rows,
        )
    except ValueError as exc:
        raise SystemExit(str(exc)) from exc
    print(
        json.dumps(
            {
                "ready": str(args.ready),
                "ready_version": ready["ready_version"],
                "binary_repo_commit": ready["binary_repo_commit"],
                "runner_repo_commit": ready["runner_repo_commit"],
                "group_count": ready["group_count"],
                "row_count": ready["row_count"],
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
