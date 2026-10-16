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
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

ADMISSION_POLICY = "pre-io+pre/continuous/post-external-v3"
DEFAULT_LOCK = Path("/run/lock/all-db-bench-performance.lock")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_status(path: Path, **fields: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {"updated_at": datetime.now(timezone.utc).isoformat(), "pid": os.getpid(), **fields}
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
    os.replace(tmp, path)


def checked_repo_commit(repo: Path) -> str:
    dirty = subprocess.check_output(
        ["git", "status", "--porcelain", "--untracked-files=no"], cwd=repo, text=True
    ).strip()
    if dirty:
        raise ValueError("generation runner checkout has tracked modifications")
    return subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()


def checked_binary(path: Path, expected_sha: str, *, label: str) -> Path:
    if not path.is_file() or not os.access(path, os.X_OK):
        raise ValueError(f"{label} binary is not executable: {path}")
    actual = sha256(path)
    if actual != expected_sha:
        raise ValueError(f"{label} binary SHA-256 mismatch: expected {expected_sha}, got {actual}")
    return path


@dataclass(frozen=True)
class Pins:
    commit: str
    kvbench: Path
    kvbench_sha256: str
    recordbench: Path
    recordbench_sha256: str
    rocks_recordbench: Path
    rocks_recordbench_sha256: str


def load_pins(repo: Path, kv_manifest: Path, record_manifest: Path) -> Pins:
    commit = checked_repo_commit(repo)
    kv = json.loads(kv_manifest.read_text())
    record = json.loads(record_manifest.read_text())
    if kv.get("repo_commit") != commit or record.get("repo_commit") != commit:
        raise ValueError("binary manifests and generation runner must share one exact repo commit")
    kv_item = kv.get("kv")
    record_items = record.get("binaries")
    if not isinstance(kv_item, dict) or not isinstance(record_items, dict):
        raise ValueError("unexpected binary manifest schema")
    record_item = record_items.get("recordbench")
    rocks_item = record_items.get("surrealdb-rocksdb-recordbench")
    if not isinstance(record_item, dict) or not isinstance(rocks_item, dict):
        raise ValueError("record manifest is missing baseline binaries")
    return Pins(
        commit=commit,
        kvbench=checked_binary(Path(str(kv_item.get("path", ""))), str(kv_item.get("sha256", "")), label="kvbench"),
        kvbench_sha256=str(kv_item["sha256"]),
        recordbench=checked_binary(Path(str(record_item.get("path", ""))), str(record_item.get("sha256", "")), label="recordbench"),
        recordbench_sha256=str(record_item["sha256"]),
        rocks_recordbench=checked_binary(Path(str(rocks_item.get("path", ""))), str(rocks_item.get("sha256", "")), label="surrealdb-rocksdb-recordbench"),
        rocks_recordbench_sha256=str(rocks_item["sha256"]),
    )


@dataclass(frozen=True)
class GenerationPaths:
    root: Path
    plans: Path
    status: Path
    selected: Path
    ready: Path
    kv_stock_id: str
    record_stock_id: str
    kv_resize_prefix: str
    record_resize_prefix: str
    kv_audit: Path
    record_audit: Path
    kv_quality_initial: Path
    record_quality_initial: Path
    kv_quality_refined: Path
    record_quality_refined: Path


def generation_paths(root: Path, generation_id: str) -> GenerationPaths:
    plans = root / "plans"
    return GenerationPaths(
        root=root,
        plans=plans,
        status=root / "status",
        selected=root / "selected",
        ready=root / "ready",
        kv_stock_id=f"{generation_id}-kv-stock",
        record_stock_id=f"{generation_id}-record-stock",
        kv_resize_prefix=f"{generation_id}-kv-resize",
        record_resize_prefix=f"{generation_id}-record-resize",
        kv_audit=plans / f"{generation_id}-kv-sizing.json",
        record_audit=plans / f"{generation_id}-record-sizing.json",
        kv_quality_initial=plans / f"{generation_id}-kv-quality-initial.json",
        record_quality_initial=plans / f"{generation_id}-record-quality-initial.json",
        kv_quality_refined=plans / f"{generation_id}-kv-quality-refined.json",
        record_quality_refined=plans / f"{generation_id}-record-quality-refined.json",
    )


def script(repo: Path, name: str) -> str:
    path = repo / "scripts" / name
    if not path.is_file() or not os.access(path, os.X_OK):
        raise ValueError(f"generation script is not executable: {path}")
    return str(path)


def stages(
    repo: Path,
    paths: GenerationPaths,
    pins: Pins,
    kv_manifest: Path,
    record_manifest: Path,
    lock_file: Path,
    *,
    watch: bool,
    busy_sleep: float,
) -> list[tuple[str, list[str]]]:
    run_root = repo / "results" / "runs"
    watch_args = ["--watch", "--busy-sleep", str(busy_sleep)] if watch else []
    kv_stock = run_root / paths.kv_stock_id
    record_stock = run_root / paths.record_stock_id
    kv_selected = paths.selected / "kv"
    record_selected = paths.selected / "record"

    result: list[tuple[str, list[str]]] = [
        (
            "kv-stock",
            [
                script(repo, "run-kv-quick-idle.py"), "--repo", str(repo),
                "--bench-bin", str(pins.kvbench), "--expected-bench-sha256", pins.kvbench_sha256,
                "--run-id", paths.kv_stock_id, "--lock-file", str(lock_file),
                "--status-file", str(paths.status / "kv-stock.json"),
                "--audit-dir", str(paths.plans), "--audit-json", str(paths.kv_audit),
                "--audit-markdown", str(paths.kv_audit.with_suffix(".md")), *watch_args,
            ],
        ),
        (
            "record-stock",
            [
                script(repo, "run-record-quick-idle.py"), "--repo", str(repo),
                "--bench-bin", str(pins.recordbench), "--rocks-bench-bin", str(pins.rocks_recordbench),
                "--expected-bench-sha256", pins.recordbench_sha256,
                "--expected-rocks-bench-sha256", pins.rocks_recordbench_sha256,
                "--run-id", paths.record_stock_id, "--lock-file", str(lock_file),
                "--status-file", str(paths.status / "record-stock.json"),
                "--audit-dir", str(paths.plans), "--audit-json", str(paths.record_audit),
                "--audit-markdown", str(paths.record_audit.with_suffix(".md")), *watch_args,
            ],
        ),
        (
            "kv-quality-plan",
            [
                script(repo, "audit-stock-short-pressure.py"), str(kv_stock), str(paths.kv_audit),
                "--lane", "kv", "--json-out", str(paths.kv_quality_initial),
                "--markdown-out", str(paths.kv_quality_initial.with_suffix(".md")),
            ],
        ),
        (
            "record-quality-plan",
            [
                script(repo, "audit-stock-short-pressure.py"), str(record_stock), str(paths.record_audit),
                "--lane", "record", "--json-out", str(paths.record_quality_initial),
                "--markdown-out", str(paths.record_quality_initial.with_suffix(".md")),
            ],
        ),
        (
            "kv-followups-initial",
            [
                script(repo, "run-kv-sizing-followups.py"), "--repo", str(repo),
                "--plan", str(paths.kv_audit), "--quality-plan", str(paths.kv_quality_initial),
                "--run-prefix", paths.kv_resize_prefix, "--bench-bin", str(pins.kvbench),
                "--expected-bench-sha256", pins.kvbench_sha256, "--lock-file", str(lock_file),
                "--status-file", str(paths.status / "kv-followups-initial.json"), *watch_args,
            ],
        ),
        (
            "record-followups-initial",
            [
                script(repo, "run-record-sizing-followups.py"), "--repo", str(repo),
                "--plan", str(paths.record_audit), "--quality-plan", str(paths.record_quality_initial),
                "--run-prefix", paths.record_resize_prefix, "--bench-bin", str(pins.recordbench),
                "--rocks-bench-bin", str(pins.rocks_recordbench),
                "--expected-bench-sha256", pins.recordbench_sha256,
                "--expected-rocks-bench-sha256", pins.rocks_recordbench_sha256,
                "--lock-file", str(lock_file), "--status-file", str(paths.status / "record-followups-initial.json"),
                *watch_args,
            ],
        ),
        (
            "kv-refine-quality",
            [
                script(repo, "refine-stock-quality-repairs.py"), str(paths.kv_quality_initial),
                str(paths.kv_audit), str(run_root), "--resize-prefix", paths.kv_resize_prefix,
                "--json-out", str(paths.kv_quality_refined),
                "--markdown-out", str(paths.kv_quality_refined.with_suffix(".md")),
            ],
        ),
        (
            "record-refine-quality",
            [
                script(repo, "refine-stock-quality-repairs.py"), str(paths.record_quality_initial),
                str(paths.record_audit), str(run_root), "--resize-prefix", paths.record_resize_prefix,
                "--json-out", str(paths.record_quality_refined),
                "--markdown-out", str(paths.record_quality_refined.with_suffix(".md")),
            ],
        ),
        (
            "kv-followups-refined",
            [
                script(repo, "run-kv-sizing-followups.py"), "--repo", str(repo),
                "--plan", str(paths.kv_audit), "--quality-plan", str(paths.kv_quality_refined),
                "--run-prefix", paths.kv_resize_prefix, "--bench-bin", str(pins.kvbench),
                "--expected-bench-sha256", pins.kvbench_sha256, "--lock-file", str(lock_file),
                "--status-file", str(paths.status / "kv-followups-refined.json"), *watch_args,
            ],
        ),
        (
            "record-followups-refined",
            [
                script(repo, "run-record-sizing-followups.py"), "--repo", str(repo),
                "--plan", str(paths.record_audit), "--quality-plan", str(paths.record_quality_refined),
                "--run-prefix", paths.record_resize_prefix, "--bench-bin", str(pins.recordbench),
                "--rocks-bench-bin", str(pins.rocks_recordbench),
                "--expected-bench-sha256", pins.recordbench_sha256,
                "--expected-rocks-bench-sha256", pins.rocks_recordbench_sha256,
                "--lock-file", str(lock_file), "--status-file", str(paths.status / "record-followups-refined.json"),
                *watch_args,
            ],
        ),
        (
            "finalize",
            [
                script(repo, "finalize-baseline-sizing.py"), "--repo", str(repo),
                "--kv-stock-results", str(kv_stock / "results.ndjson"), "--kv-audit", str(paths.kv_audit),
                "--kv-resize-prefix", paths.kv_resize_prefix, "--kv-out-dir", str(kv_selected),
                "--kv-quality-repair-plan", str(paths.kv_quality_refined),
                "--record-stock-results", str(record_stock / "results.ndjson"), "--record-audit", str(paths.record_audit),
                "--record-resize-prefix", paths.record_resize_prefix, "--record-out-dir", str(record_selected),
                "--record-quality-repair-plan", str(paths.record_quality_refined),
            ],
        ),
        (
            "verify-kv",
            [
                script(repo, "verify-kv-ready.py"), "--selected", str(kv_selected),
                "--binary-manifest", str(kv_manifest), "--expected-commit", pins.commit,
                "--repo", str(repo), "--stock-run-id", paths.kv_stock_id,
                "--expected-admission-policy", ADMISSION_POLICY, "--out", str(paths.ready / "kv.json"),
            ],
        ),
        (
            "verify-record",
            [
                script(repo, "verify-record-ready.py"), "--selected", str(record_selected),
                "--binary-manifest", str(record_manifest), "--expected-commit", pins.commit,
                "--repo", str(repo), "--stock-run-id", paths.record_stock_id,
                "--expected-admission-policy", ADMISSION_POLICY, "--out", str(paths.ready / "record.json"),
            ],
        ),
    ]
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description="Run one provenance-isolated KV + record baseline publication generation")
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--generation-root", type=Path, required=True)
    parser.add_argument("--generation-id", required=True)
    parser.add_argument("--kv-binary-manifest", type=Path, required=True)
    parser.add_argument("--record-binary-manifest", type=Path, required=True)
    parser.add_argument("--lock-file", type=Path, default=DEFAULT_LOCK)
    parser.add_argument("--watch", action="store_true")
    parser.add_argument("--busy-sleep", type=float, default=2.0)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    try:
        pins = load_pins(args.repo, args.kv_binary_manifest, args.record_binary_manifest)
        paths = generation_paths(args.generation_root, args.generation_id)
        for directory in (paths.plans, paths.status, paths.selected, paths.ready):
            directory.mkdir(parents=True, exist_ok=True)
        planned = stages(
            args.repo, paths, pins, args.kv_binary_manifest, args.record_binary_manifest,
            args.lock_file, watch=args.watch, busy_sleep=args.busy_sleep,
        )
    except (OSError, ValueError, json.JSONDecodeError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from error

    if args.dry_run:
        print(json.dumps({"commit": pins.commit, "generation_id": args.generation_id, "stages": [{"name": name, "argv": argv} for name, argv in planned]}, indent=2))
        return 0

    overall_status = paths.status / "baseline-generation.json"
    total = len(planned)
    for index, (name, argv) in enumerate(planned, start=1):
        write_status(overall_status, state="running", generation_id=args.generation_id, commit=pins.commit, stage=name, stage_index=index, stage_count=total)
        proc = subprocess.run(argv, cwd=args.repo, check=False)
        if proc.returncode != 0:
            state = "waiting-for-idle" if proc.returncode == 75 else "failed"
            write_status(overall_status, state=state, generation_id=args.generation_id, commit=pins.commit, stage=name, stage_index=index, stage_count=total, returncode=proc.returncode)
            return proc.returncode
    write_status(overall_status, state="complete", generation_id=args.generation_id, commit=pins.commit, stage_count=total, kv_ready=str(paths.ready / "kv.json"), record_ready=str(paths.ready / "record.json"))
    print(f"baseline generation complete: {args.generation_id}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
