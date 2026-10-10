#!/usr/bin/env python3
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
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def checked_runner_commit(repo: Path) -> str:
    tracked_dirty = subprocess.check_output(
        ["git", "status", "--porcelain", "--untracked-files=no"], cwd=repo, text=True
    ).strip()
    if tracked_dirty:
        raise SystemExit("runner checkout has tracked modifications; refusing readiness certificate")
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
        raise SystemExit(
            "selected source-run provenance mismatch: "
            f"manifest={expected} runs={len(selected)}"
        )
    return sorted(selected)


def load_supports(
    repo: Path,
    run_ids: list[str],
    *,
    expected_admission_policy: str,
) -> tuple[dict[str, str], str, dict[str, Any] | None]:
    supports: dict[str, dict[str, Any]] = {}
    support_hashes: dict[str, str] = {}
    for run_id in run_ids:
        support_path = repo / "results" / "runs" / run_id / "support.json"
        if not support_path.is_file():
            raise SystemExit(f"missing source support: {support_path}")
        support = json.loads(support_path.read_text())
        if support.get("admission_policy") != expected_admission_policy:
            raise SystemExit(
                f"unexpected admission policy for {run_id}: {support.get('admission_policy')!r}"
            )
        supports[run_id] = support
        support_hashes[run_id] = sha256(support_path)

    noise_guards = {support.get("noise_guard_sha256") for support in supports.values()}
    if None in noise_guards or "" in noise_guards or len(noise_guards) != 1:
        raise SystemExit(
            "selected performance sources have inconsistent noise guards: "
            f"{sorted(map(str, noise_guards))}"
        )
    selected_noise_guard_sha256 = str(next(iter(noise_guards)))

    continuous_identity: dict[str, Any] | None = None
    if expected_admission_policy == CONTINUOUS_ADMISSION_POLICY:
        identities: set[str] = set()
        decoded: dict[str, dict[str, Any]] = {}
        for run_id, support in supports.items():
            identity = {
                "guard_sha256": support.get("continuous_noise_guard_sha256"),
                "sample_ms": support.get("continuous_noise_sample_ms"),
                "max_cpu_percent": support.get("continuous_noise_max_cpu_percent"),
                "max_io_bytes": support.get("continuous_noise_max_io_bytes"),
                "max_io_rate_mib_s": support.get("continuous_noise_max_io_rate_mib_s"),
            }
            if any(value is None or value == "" for value in identity.values()):
                raise SystemExit(f"missing continuous admission provenance for {run_id}")
            encoded = json.dumps(identity, sort_keys=True, separators=(",", ":"))
            identities.add(encoded)
            decoded[encoded] = identity
        if len(identities) != 1:
            raise SystemExit("selected performance sources have inconsistent continuous admission provenance")
        continuous_identity = decoded[next(iter(identities))]

    return support_hashes, selected_noise_guard_sha256, continuous_identity


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--selected", type=Path, required=True)
    ap.add_argument("--binary-manifest", type=Path, required=True)
    ap.add_argument("--expected-commit", required=True)
    ap.add_argument("--repo", type=Path, required=True)
    ap.add_argument("--stock-run-id", required=True)
    ap.add_argument("--expected-admission-policy", required=True)
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()

    runner_repo_commit = checked_runner_commit(a.repo)
    manifest = json.loads((a.selected / "manifest.json").read_text())
    summary = json.loads((a.selected / "summary.json").read_text())
    bin_manifest = json.loads(a.binary_manifest.read_text())
    rows = [
        json.loads(line)
        for line in (a.selected / "results.ndjson").read_text().splitlines()
        if line.strip()
    ]
    if not manifest.get("complete") or int(manifest.get("selected_groups", -1)) != 40:
        raise SystemExit("record selection is not complete at 40 groups")
    if int(summary.get("group_count", -1)) != 40 or summary.get("problems"):
        raise SystemExit("record summary is not a clean 40-group corpus")
    if int(summary.get("row_count", -1)) != len(rows) or int(manifest.get("selected_rows", -1)) != len(rows):
        raise SystemExit("record row-count provenance mismatch")
    reads = {r.get("read_materialization") for r in rows}
    writes = {r.get("write_materialization") for r in rows}
    if reads != {"full-record-v1"}:
        raise SystemExit(f"unexpected read semantics: {sorted(map(str, reads))}")
    if writes != {"no-return-v1"}:
        raise SystemExit(f"unexpected write semantics: {sorted(map(str, writes))}")

    run_ids = selected_run_ids(manifest, a.stock_run_id)
    source_supports, selected_noise_guard_sha256, continuous_identity = load_supports(
        a.repo,
        run_ids,
        expected_admission_policy=a.expected_admission_policy,
    )

    if bin_manifest.get("repo_commit") != a.expected_commit:
        raise SystemExit("binary manifest commit mismatch")
    if (
        bin_manifest.get("read_materialization") != "full-record-v1"
        or bin_manifest.get("write_materialization") != "no-return-v1"
    ):
        raise SystemExit("binary manifest semantic mismatch")
    binaries = bin_manifest.get("binaries")
    if not isinstance(binaries, dict) or len(binaries) != 6:
        raise SystemExit("binary manifest does not describe exactly six record binaries")
    for name, item in binaries.items():
        path = Path(item.get("path", ""))
        expected = item.get("sha256")
        if not path.is_file() or not expected or sha256(path) != expected:
            raise SystemExit(f"pinned binary verification failed: {name}")

    out: dict[str, Any] = {
        "ready_version": 5 if continuous_identity is not None else 4,
        "repo_commit": a.expected_commit,
        "binary_repo_commit": a.expected_commit,
        "runner_repo_commit": runner_repo_commit,
        "admission_policy": a.expected_admission_policy,
        "source_support_sha256": source_supports,
        "selected_noise_guard_sha256": selected_noise_guard_sha256,
        "group_count": 40,
        "row_count": len(rows),
        "read_materialization": "full-record-v1",
        "write_materialization": "no-return-v1",
        "results_sha256": sha256(a.selected / "results.ndjson"),
        "selection_manifest_sha256": sha256(a.selected / "manifest.json"),
        "summary_sha256": sha256(a.selected / "summary.json"),
        "binary_manifest_sha256": sha256(a.binary_manifest),
    }
    if continuous_identity is not None:
        out.update(
            {
                "selected_continuous_noise_guard_sha256": continuous_identity["guard_sha256"],
                "continuous_noise_sample_ms": continuous_identity["sample_ms"],
                "continuous_noise_max_cpu_percent": continuous_identity["max_cpu_percent"],
                "continuous_noise_max_io_bytes": continuous_identity["max_io_bytes"],
                "continuous_noise_max_io_rate_mib_s": continuous_identity["max_io_rate_mib_s"],
            }
        )

    a.out.parent.mkdir(parents=True, exist_ok=True)
    tmp = a.out.with_suffix(a.out.suffix + ".tmp")
    tmp.write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")
    os.replace(tmp, a.out)
    print(json.dumps(out, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
