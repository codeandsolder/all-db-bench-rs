#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

from concurrency_support_provenance import canonical_identity_sha256, support_identity


def verify(
    support_path: Path,
    plan_path: Path,
    repo: Path,
    *,
    runner: str,
    lane: str,
) -> dict[str, Any]:
    support = json.loads(support_path.read_text())
    plan = json.loads(plan_path.read_text())
    identity = support_identity(
        support,
        expected_lane=lane,
        repo=repo,
        runner_name=runner,
        label=str(support_path),
    )
    expected = plan.get("expected_measurement_identity")
    if expected is not None and identity != expected:
        raise ValueError(
            "concurrency measurement identity differs from plan expectation: "
            f"actual={canonical_identity_sha256(identity)} "
            f"expected={canonical_identity_sha256(expected)}"
        )
    return identity


def main() -> int:
    parser = argparse.ArgumentParser(description="Verify one concurrency run support identity against its plan")
    parser.add_argument("--support", type=Path, required=True)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--runner", required=True)
    parser.add_argument("--lane", choices=("kv-concurrency", "record-concurrency"), required=True)
    args = parser.parse_args()
    try:
        identity = verify(
            args.support,
            args.plan,
            args.repo,
            runner=args.runner,
            lane=args.lane,
        )
    except (OSError, ValueError, json.JSONDecodeError) as error:
        raise SystemExit(str(error)) from error
    print(json.dumps({"measurement_identity_sha256": canonical_identity_sha256(identity)}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
