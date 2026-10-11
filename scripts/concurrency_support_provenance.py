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
    repo: Path | None = None,
    runner_name: str | None = None,
    expected_admission: str = CONTINUOUS_ADMISSION,
    label: str = "support",
) -> dict[str, Any]:
    if support.get("lane") != expected_lane:
        raise ValueError(f"unexpected lane in {label}: {support.get('lane')!r}")
    if support.get("profile") != "quick":
        raise ValueError(f"unexpected profile in {label}: {support.get('profile')!r}")
    if support.get("admission_policy") != expected_admission:
        raise ValueError(
            f"unexpected admission policy in {label}: {support.get('admission_policy')!r}; "
            f"expected {expected_admission!r}"
        )

    identity: dict[str, Any] = {
        "lane": expected_lane,
        "profile": "quick",
        "admission_policy": expected_admission,
    }
    text_keys = (
        "runner_sha256",
        "concurrency_runner_common_sha256",
        "continuous_noise_common_sha256",
        "concurrency_policy_sha256",
        "noise_guard_sha256",
        "continuous_noise_guard_sha256",
        "benchmark_binary_sha256",
        "build_profile",
        "hostname",
        "machine_id_sha256",
        "filesystem",
        "source",
        "benchmark_source_commit",
        "harness_commit",
    )
    for key in text_keys:
        identity[key] = str(_required(support, key, label))

    for key in (
        "runner_sha256",
        "concurrency_runner_common_sha256",
        "continuous_noise_common_sha256",
        "concurrency_policy_sha256",
        "noise_guard_sha256",
        "continuous_noise_guard_sha256",
        "benchmark_binary_sha256",
        "machine_id_sha256",
    ):
        if not HEX64.fullmatch(identity[key]):
            raise ValueError(f"malformed {key} in {label}: {identity[key]!r}")
    for key in ("benchmark_source_commit", "harness_commit"):
        if not HEX40.fullmatch(identity[key]):
            raise ValueError(f"malformed {key} in {label}: {identity[key]!r}")
    if identity["build_profile"] != "external":
        raise ValueError(f"unexpected build_profile in {label}: {identity['build_profile']!r}")

    for key in (
        "continuous_noise_sample_ms",
        "continuous_noise_max_cpu_percent",
        "continuous_noise_max_io_average_mib_s",
        "continuous_noise_max_io_rate_mib_s",
        "initial_min_free_gib",
        "case_min_free_gib",
        "case_timeout_s",
    ):
        identity[key] = _positive(_required(support, key, label), key, label)
    for key, expected in EXPECTED_CONTINUOUS.items():
        if identity[key] != expected:
            raise ValueError(f"unexpected {key} in {label}: {identity[key]} != {expected}")
    if identity["initial_min_free_gib"] < 10 or identity["case_min_free_gib"] < 10:
        raise ValueError(f"concurrency free-space floor below 10 GiB in {label}")

    if expected_lane == "kv-concurrency":
        identity["persy_lock_timeout_ms"] = _positive(
            _required(support, "persy_lock_timeout_ms", label), "persy_lock_timeout_ms", label
        )
        if identity["persy_lock_timeout_ms"] != 250:
            raise ValueError(f"unexpected persy_lock_timeout_ms in {label}")
        protocol = str(_required(support, "prepared_db_protocol", label))
        if protocol != "case-private-clean-close-v1":
            raise ValueError(f"unexpected prepared_db_protocol in {label}: {protocol!r}")
        identity["prepared_db_protocol"] = protocol
    elif expected_lane == "record-concurrency":
        for key in (
            "surrealdb_rocksdb_binary_sha256",
            "surrealdb_rocksdb_source_commit",
            "rocks_build_profile",
            "read_materialization",
            "write_materialization",
        ):
            identity[key] = str(_required(support, key, label))
        if not HEX64.fullmatch(identity["surrealdb_rocksdb_binary_sha256"]):
            raise ValueError(f"malformed surrealdb_rocksdb_binary_sha256 in {label}")
        if not HEX40.fullmatch(identity["surrealdb_rocksdb_source_commit"]):
            raise ValueError(f"malformed surrealdb_rocksdb_source_commit in {label}")
        if identity["rocks_build_profile"] != "external":
            raise ValueError(f"unexpected rocks_build_profile in {label}")
        if identity["read_materialization"] != "full-record-v1":
            raise ValueError(f"unexpected read_materialization in {label}")
        if identity["write_materialization"] != "no-return-v1":
            raise ValueError(f"unexpected write_materialization in {label}")
    else:
        raise ValueError(f"unsupported concurrency lane: {expected_lane!r}")

    if repo is not None:
        if runner_name is None:
            raise ValueError("runner_name is required when repo is provided")
        expected_files = {
            "runner_sha256": repo / "scripts" / runner_name,
            "concurrency_runner_common_sha256": repo / "scripts" / "concurrency-runner-common.sh",
            "continuous_noise_common_sha256": repo / "scripts" / "continuous-noise-runner-common.sh",
            "concurrency_policy_sha256": repo / "scripts" / "concurrency-matrix-policy.sh",
            "noise_guard_sha256": repo / "scripts" / "check-external-noise.py",
            "continuous_noise_guard_sha256": repo / "scripts" / "run-with-continuous-noise.py",
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


def canonical_identity_sha256(identity: dict[str, Any]) -> str:
    payload = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(payload).hexdigest()
