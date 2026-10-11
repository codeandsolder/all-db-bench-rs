from __future__ import annotations

import hashlib
import json
import re
import subprocess
from pathlib import Path
from typing import Any

CONTINUOUS_ADMISSION = "pre-io+pre/continuous/post-external-v3"
HEX64 = re.compile(r"^[0-9a-f]{64}$")
HEX40 = re.compile(r"^[0-9a-f]{40}$")
EXPECTED_CONTINUOUS = {
    "continuous_noise_sample_ms": 250.0,
    "continuous_noise_max_cpu_percent": 50.0,
    "continuous_noise_max_io_average_mib_s": 2.0,
    "continuous_noise_max_io_rate_mib_s": 8.0,
}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_head(repo: Path) -> str:
    return subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()


def _required(support: dict[str, Any], key: str, label: str) -> Any:
    value = support.get(key)
    if value in (None, ""):
        raise ValueError(f"missing {key} in {label}")
    return value


def _positive(value: Any, key: str, label: str) -> float:
    try:
        result = float(value)
    except (TypeError, ValueError) as error:
        raise ValueError(f"invalid {key} in {label}: {value!r}") from error
    if result <= 0:
        raise ValueError(f"non-positive {key} in {label}: {value!r}")
    return result


def support_identity(
    support: dict[str, Any],
    *,
    expected_lane: str,
    repo: Path,
    runner_name: str,
    min_free_gib: float,
    expected_cache_mode: str | None = None,
    label: str = "support",
) -> dict[str, Any]:
    if support.get("lane") != expected_lane:
        raise ValueError(f"unexpected lane in {label}: {support.get('lane')!r}")
    if support.get("profile") != "quick":
        raise ValueError(f"unexpected profile in {label}: {support.get('profile')!r}")
    if support.get("admission_policy") != CONTINUOUS_ADMISSION:
        raise ValueError(f"unexpected admission policy in {label}: {support.get('admission_policy')!r}")

    identity: dict[str, Any] = {
        "lane": expected_lane,
        "profile": "quick",
        "admission_policy": CONTINUOUS_ADMISSION,
    }
    for key in (
        "runner_sha256",
        "performance_common_sha256",
        "continuous_noise_common_sha256",
        "noise_guard_sha256",
        "continuous_noise_guard_sha256",
        "benchmark_binary_sha256",
        "benchmark_source_commit",
        "harness_commit",
        "build_profile",
        "hostname",
        "machine_id_sha256",
        "filesystem",
        "source",
        "resume_order_policy",
    ):
        identity[key] = str(_required(support, key, label))

    policy_key: str
    policy_file: str
    if expected_lane in {"kv-sustained", "record-sustained"}:
        policy_key, policy_file = "sustained_policy_sha256", "sustained-matrix-policy.sh"
    elif expected_lane == "kv-reopen":
        policy_key, policy_file = "kv_matrix_policy_sha256", "kv-matrix-policy.sh"
    else:
        raise ValueError(f"unsupported performance lane: {expected_lane!r}")
    identity[policy_key] = str(_required(support, policy_key, label))

    for key in (
        "runner_sha256",
        "performance_common_sha256",
        "continuous_noise_common_sha256",
        "noise_guard_sha256",
        "continuous_noise_guard_sha256",
        "benchmark_binary_sha256",
        policy_key,
        "machine_id_sha256",
    ):
        if not HEX64.fullmatch(identity[key]):
            raise ValueError(f"malformed {key} in {label}: {identity[key]!r}")
    for key in ("benchmark_source_commit", "harness_commit"):
        if not HEX40.fullmatch(identity[key]):
            raise ValueError(f"malformed {key} in {label}: {identity[key]!r}")
    if identity["build_profile"] != "external":
        raise ValueError(f"unexpected build_profile in {label}: {identity['build_profile']!r}")
    if identity["resume_order_policy"] != "reshuffle-remaining":
        raise ValueError(f"unexpected resume_order_policy in {label}: {identity['resume_order_policy']!r}")

    for key in (
        "continuous_noise_sample_ms",
        "continuous_noise_max_cpu_percent",
        "continuous_noise_max_io_average_mib_s",
        "continuous_noise_max_io_rate_mib_s",
        "initial_min_free_gib",
        "case_min_free_gib",
    ):
        identity[key] = _positive(_required(support, key, label), key, label)
    for key, expected in EXPECTED_CONTINUOUS.items():
        if identity[key] != expected:
            raise ValueError(f"unexpected {key} in {label}: {identity[key]} != {expected}")
    if identity["initial_min_free_gib"] < min_free_gib or identity["case_min_free_gib"] < min_free_gib:
        raise ValueError(f"free-space floor below {min_free_gib:g} GiB in {label}")

    if expected_lane == "record-sustained":
        for key in (
            "rocksdb_benchmark_binary_sha256",
            "surrealdb_rocksdb_source_commit",
            "rocks_build_profile",
            "read_materialization",
            "write_materialization",
        ):
            identity[key] = str(_required(support, key, label))
        if not HEX64.fullmatch(identity["rocksdb_benchmark_binary_sha256"]):
            raise ValueError(f"malformed rocksdb_benchmark_binary_sha256 in {label}")
        if not HEX40.fullmatch(identity["surrealdb_rocksdb_source_commit"]):
            raise ValueError(f"malformed surrealdb_rocksdb_source_commit in {label}")
        if identity["rocks_build_profile"] != "external":
            raise ValueError(f"unexpected rocks_build_profile in {label}")
        if identity["read_materialization"] != "full-record-v1":
            raise ValueError(f"unexpected read_materialization in {label}")
        if identity["write_materialization"] != "no-return-v1":
            raise ValueError(f"unexpected write_materialization in {label}")
    elif expected_lane == "kv-reopen":
        cache_mode = str(_required(support, "cache_mode", label))
        if expected_cache_mode is not None and cache_mode != expected_cache_mode:
            raise ValueError(f"unexpected cache_mode in {label}: {cache_mode!r}")
        identity["cache_mode"] = cache_mode

    expected_files = {
        "runner_sha256": repo / "scripts" / runner_name,
        "performance_common_sha256": repo / "scripts" / "performance-runner-common.sh",
        "continuous_noise_common_sha256": repo / "scripts" / "continuous-noise-runner-common.sh",
        "noise_guard_sha256": repo / "scripts" / "check-external-noise.py",
        "continuous_noise_guard_sha256": repo / "scripts" / "run-with-continuous-noise.py",
        policy_key: repo / "scripts" / policy_file,
    }
    for key, path in expected_files.items():
        actual = sha256(path)
        if identity[key] != actual:
            raise ValueError(f"runtime provenance mismatch for {key} in {label}: {identity[key]} != {actual}")
    head = git_head(repo)
    if identity["harness_commit"] != head:
        raise ValueError(
            f"runtime provenance mismatch for harness_commit in {label}: "
            f"{identity['harness_commit']} != {head}"
        )
    return identity


def verify_binary_manifest(identity: dict[str, Any], manifest: dict[str, Any]) -> None:
    lane = identity["lane"]
    if lane in {"kv-sustained", "kv-reopen"}:
        item = manifest.get("kv", {})
        if identity["benchmark_binary_sha256"] != item.get("sha256"):
            raise ValueError("primary binary SHA differs from pinned manifest")
        expected_source = (
            manifest.get("source_commits", {}).get("kv")
            if lane == "kv-sustained"
            else manifest.get("repo_commit")
        )
        if identity["benchmark_source_commit"] != expected_source:
            raise ValueError("primary binary source commit differs from pinned manifest")
    elif lane == "record-sustained":
        record = manifest.get("record", {})
        rocks = manifest.get("record_rocksdb", {})
        sources = manifest.get("source_commits", {})
        if identity["benchmark_binary_sha256"] != record.get("sha256"):
            raise ValueError("record binary SHA differs from pinned manifest")
        if identity["rocksdb_benchmark_binary_sha256"] != rocks.get("sha256"):
            raise ValueError("record RocksDB binary SHA differs from pinned manifest")
        if identity["benchmark_source_commit"] != sources.get("record"):
            raise ValueError("record binary source commit differs from pinned manifest")
        if identity["surrealdb_rocksdb_source_commit"] != sources.get("record_rocksdb"):
            raise ValueError("record RocksDB source commit differs from pinned manifest")
        if identity["read_materialization"] != manifest.get("read_materialization"):
            raise ValueError("record read materialization differs from pinned manifest")
        if identity["write_materialization"] != manifest.get("write_materialization"):
            raise ValueError("record write materialization differs from pinned manifest")
    else:
        raise ValueError(f"unsupported manifest lane: {lane!r}")


def canonical_identity_sha256(identity: dict[str, Any]) -> str:
    return hashlib.sha256(json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
