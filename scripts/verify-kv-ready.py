#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
from pathlib import Path
from typing import Any

CONTINUOUS_ADMISSION_POLICY = "pre-io+pre/continuous/post-external-v3"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def checked_runner_commit(repo: Path) -> str:
    tracked_dirty = subprocess.check_output(
        ["git", "status", "--porcelain", "--untracked-files=no"], cwd=repo, text=True
    ).strip()
    if tracked_dirty:
        raise ValueError("runner checkout has tracked modifications; refusing readiness certificate")
    return subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()


def selected_run_ids(manifest: dict[str, Any], stock_run_id: str) -> list[str]:
    selected: set[str] = set()
    selected_stock_groups = int(manifest.get("selected_stock_groups", 0))
    if selected_stock_groups > 0:
        selected.add(stock_run_id)
    selected.update(
        str(item["run_id"])
        for item in manifest.get("sources", [])
        if item.get("source") in {"resize", "quality-repair"} and item.get("run_id")
    )
    expected = (
        (1 if selected_stock_groups > 0 else 0)
        + int(manifest.get("selected_resize_groups", 0))
        + int(manifest.get("selected_quality_repair_groups", 0))
    )
    if len(selected) != expected:
        raise ValueError(
            "selected source-run provenance mismatch: "
            f"manifest={expected} runs={len(selected)}"
        )
    return sorted(selected)


def source_identity(repo: Path, support: dict[str, Any]) -> dict[str, Any]:
    file_fields = {
        "runner_sha256": repo / "scripts" / "run-kv-matrix.sh",
        "kv_matrix_policy_sha256": repo / "scripts" / "kv-matrix-policy.sh",
        "performance_common_sha256": repo / "scripts" / "performance-runner-common.sh",
        "continuous_noise_common_sha256": repo / "scripts" / "continuous-noise-runner-common.sh",
        "noise_guard_sha256": repo / "scripts" / "check-external-noise.py",
        "continuous_noise_guard_sha256": repo / "scripts" / "run-with-continuous-noise.py",
    }
    for field, path in file_fields.items():
        if support.get(field) != sha256(path):
            raise ValueError(f"KV runtime provenance mismatch: {field}")
    return {
        **{field: support[field] for field in file_fields},
        "admission_policy": support.get("admission_policy"),
        "continuous_noise_sample_ms": support.get("continuous_noise_sample_ms"),
        "continuous_noise_max_cpu_percent": support.get("continuous_noise_max_cpu_percent"),
        "continuous_noise_max_io_average_mib_s": support.get("continuous_noise_max_io_average_mib_s"),
        "continuous_noise_max_io_rate_mib_s": support.get("continuous_noise_max_io_rate_mib_s"),
        "hostname": support.get("hostname"),
        "machine_id_sha256": support.get("machine_id_sha256"),
        "filesystem": support.get("filesystem"),
        "source": support.get("source"),
    }


def verify(
    selected: Path,
    binary_manifest: Path,
    expected_commit: str,
    repo: Path,
    stock_run_id: str,
    expected_admission_policy: str,
) -> dict[str, Any]:
    manifest_path = selected / "manifest.json"
    summary_path = selected / "summary.json"
    results_path = selected / "results.ndjson"
    for path in (manifest_path, summary_path, results_path, binary_manifest):
        if not path.is_file():
            raise ValueError(f"missing KV readiness artifact: {path}")

    manifest = json.loads(manifest_path.read_text())
    summary = json.loads(summary_path.read_text())
    rows = [json.loads(line) for line in results_path.read_text().splitlines() if line.strip()]
    if not manifest.get("complete") or int(manifest.get("selected_groups", -1)) != 180:
        raise ValueError("KV selection is not complete at 180 groups")
    if int(summary.get("group_count", -1)) != 180 or summary.get("problems"):
        raise ValueError("KV summary is not a clean 180-group corpus")
    if int(summary.get("row_count", -1)) != len(rows) or int(manifest.get("selected_rows", -1)) != len(rows):
        raise ValueError("KV row-count provenance mismatch")
    if int(manifest.get("final_status_counts", {}).get("undersized", 0)) != 0:
        raise ValueError("KV selection still contains undersized groups")

    binary_data = json.loads(binary_manifest.read_text())
    if binary_data.get("repo_commit") != expected_commit:
        raise ValueError("KV binary manifest commit mismatch")
    binary = binary_data.get("kv")
    if not isinstance(binary, dict):
        raise ValueError("KV binary manifest is missing the kv binary")
    binary_path = Path(str(binary.get("path", "")))
    binary_sha = str(binary.get("sha256", ""))
    if not binary_path.is_file() or not binary_sha or sha256(binary_path) != binary_sha:
        raise ValueError("pinned KV binary verification failed")

    run_ids = selected_run_ids(manifest, stock_run_id)
    support_hashes: dict[str, str] = {}
    identities: set[str] = set()
    decoded_identities: dict[str, dict[str, Any]] = {}
    for run_id in run_ids:
        support_path = repo / "results" / "runs" / run_id / "support.json"
        if not support_path.is_file():
            raise ValueError(f"missing source support: {support_path}")
        support = json.loads(support_path.read_text())
        if support.get("admission_policy") != expected_admission_policy:
            raise ValueError(f"unexpected admission policy for {run_id}: {support.get('admission_policy')!r}")
        if support.get("benchmark_binary_sha256") != binary_sha:
            raise ValueError(f"KV benchmark binary mismatch for {run_id}")
        if support.get("build_profile") != "external":
            raise ValueError(f"KV source was not run from a pinned external binary: {run_id}")
        if float(support.get("initial_min_free_gib", 0)) < 10 or float(support.get("case_min_free_gib", 0)) < 10:
            raise ValueError(f"KV source free-space provenance is below quick-publication policy: {run_id}")
        identity = source_identity(repo, support)
        encoded = json.dumps(identity, sort_keys=True, separators=(",", ":"))
        identities.add(encoded)
        decoded_identities[encoded] = identity
        support_hashes[run_id] = sha256(support_path)

    if not run_ids:
        raise ValueError("KV selection has no source runs")
    if len(identities) != 1:
        raise ValueError("selected KV sources have inconsistent runtime/admission identities")
    identity = decoded_identities[next(iter(identities))]
    if identity["admission_policy"] != expected_admission_policy:
        raise ValueError("KV selected source admission identity mismatch")
    if expected_admission_policy == CONTINUOUS_ADMISSION_POLICY:
        required = (
            "continuous_noise_guard_sha256",
            "continuous_noise_sample_ms",
            "continuous_noise_max_cpu_percent",
            "continuous_noise_max_io_average_mib_s",
            "continuous_noise_max_io_rate_mib_s",
        )
        if any(identity.get(field) in (None, "") for field in required):
            raise ValueError("KV continuous admission provenance is incomplete")

    runner_repo_commit = checked_runner_commit(repo)
    if runner_repo_commit != expected_commit:
        raise ValueError("KV runner checkout commit does not match pinned binary commit")

    ready: dict[str, Any] = {
        "ready_version": 7,
        "repo_commit": expected_commit,
        "binary_repo_commit": expected_commit,
        "runner_repo_commit": runner_repo_commit,
        "admission_policy": expected_admission_policy,
        "group_count": 180,
        "row_count": len(rows),
        "benchmark_binary_sha256": binary_sha,
        "source_support_sha256": support_hashes,
        "results_sha256": sha256(results_path),
        "selection_manifest_sha256": sha256(manifest_path),
        "summary_sha256": sha256(summary_path),
        "binary_manifest_sha256": sha256(binary_manifest),
        **identity,
    }
    return ready


def main() -> int:
    parser = argparse.ArgumentParser(description="Verify a fresh selected raw-KV baseline corpus")
    parser.add_argument("--selected", type=Path, required=True)
    parser.add_argument("--binary-manifest", type=Path, required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--stock-run-id", required=True)
    parser.add_argument("--expected-admission-policy", default=CONTINUOUS_ADMISSION_POLICY)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    try:
        ready = verify(
            args.selected,
            args.binary_manifest,
            args.expected_commit,
            args.repo,
            args.stock_run_id,
            args.expected_admission_policy,
        )
    except (OSError, ValueError, json.JSONDecodeError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from error
    args.out.parent.mkdir(parents=True, exist_ok=True)
    tmp = args.out.with_suffix(args.out.suffix + ".tmp")
    tmp.write_text(json.dumps(ready, indent=2, sort_keys=True) + "\n")
    os.replace(tmp, args.out)
    print(json.dumps(ready, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
