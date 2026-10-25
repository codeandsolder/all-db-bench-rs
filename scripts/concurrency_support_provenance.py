from __future__ import annotations

import hashlib
import json
import subprocess
from pathlib import Path
from typing import Any

CONTINUOUS_ADMISSION = "pre-io+pre/continuous/post-external-v3"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_head(repo: Path) -> str:
    return subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()


def support_identity(
    support: dict[str, Any],
    *,
    expected_lane: str,
    repo: Path | None = None,
    runner_name: str | None = None,
    expected_admission: str = CONTINUOUS_ADMISSION,
    label: str = "support",
) -> dict[str, Any]:
    root = Path(__file__).resolve().parents[1]
    command = [
        str(root / "scripts" / "benchctl.sh"),
        "support-identity",
        "-",
        "--expected-lane",
        expected_lane,
        "--expected-admission",
        expected_admission,
    ]
    if repo is not None:
        if runner_name is None:
            raise ValueError("runner_name is required when repo is provided")
        command.extend(["--repo", str(repo), "--runner-name", runner_name])
    proc = subprocess.run(
        command,
        cwd=root,
        input=json.dumps(support, separators=(",", ":")),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if proc.returncode != 0:
        message = proc.stderr.strip() or proc.stdout.strip() or f"benchctl support-identity failed with {proc.returncode}"
        # Preserve the old public exception shape for planner/audit callers.
        raise ValueError(f"{label}: {message}")
    payload = json.loads(proc.stdout)
    identity = payload.get("measurement_identity")
    if not isinstance(identity, dict):
        raise ValueError(f"{label}: native support identity returned malformed output")
    return identity


def canonical_identity_sha256(identity: dict[str, Any]) -> str:
    payload = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(payload).hexdigest()
