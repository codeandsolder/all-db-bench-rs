#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import hashlib
import json
import shutil
from pathlib import Path
from typing import Any


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def load_json(path: Path) -> Any:
    return json.loads(path.read_text())


def require_quiet(path: Path) -> None:
    data = load_json(path)
    if data.get("quiet") is not True:
        raise RuntimeError(f"source noise evidence is not quiet: {path}")


def copy_verified(src: Path, dst: Path) -> str:
    digest = sha256(src)
    dst.parent.mkdir(parents=True, exist_ok=True)
    if dst.exists():
        if sha256(dst) != digest:
            raise RuntimeError(f"destination conflict: {dst}")
    else:
        shutil.copy2(src, dst)
    if sha256(dst) != digest:
        raise RuntimeError(f"copy verification failed: {dst}")
    return digest


def import_results(source: Path, dest: Path, excluded_engines: set[str]) -> dict[str, Any]:
    support_path = source / "support.json"
    jobs_path = source / "jobs.txt"
    if not support_path.is_file() or not jobs_path.is_file():
        raise RuntimeError("source run must contain support.json and jobs.txt")
    support = load_json(support_path)
    if support.get("lane") != "kv-concurrency" or support.get("profile") != "quick":
        raise RuntimeError("source run is not a quick kv-concurrency campaign")
    if (dest / "support.json").exists() or (dest / "jobs.txt").exists():
        raise RuntimeError("destination support/jobs already exist; import must precede v2 runner initialization")

    entries: list[dict[str, Any]] = []
    cases = sorted((source / "cases").glob("*.json"))
    for case_path in cases:
        row = load_json(case_path)
        engine = str(row.get("engine", ""))
        if engine in excluded_engines:
            continue
        case_id = case_path.stem
        before = source / "noise" / f"{case_id}.before.json"
        after = source / "noise" / f"{case_id}.after.json"
        if not before.is_file() or not after.is_file():
            raise RuntimeError(f"missing source noise evidence for {case_id}")
        require_quiet(before)
        require_quiet(after)
        result_sha = copy_verified(case_path, dest / "cases" / case_path.name)
        before_sha = copy_verified(before, dest / "noise" / before.name)
        after_sha = copy_verified(after, dest / "noise" / after.name)
        entries.append(
            {
                "case_id": case_id,
                "engine": engine,
                "trial": row.get("trial"),
                "workload": row.get("workload"),
                "clients": row.get("clients"),
                "elapsed_s": row.get("elapsed_s"),
                "result_sha256": result_sha,
                "noise_before_sha256": before_sha,
                "noise_after_sha256": after_sha,
            }
        )

    manifest = {
        "format_version": 1,
        "policy": "reuse only accepted non-Persy v1 cases; all Persy cases rerun under v2 lock-timeout/retry semantics",
        "source_run_id": source.name,
        "source_support_sha256": sha256(support_path),
        "source_jobs_sha256": sha256(jobs_path),
        "source_benchmark_binary_sha256": support.get("benchmark_binary_sha256"),
        "source_runner_sha256": support.get("runner_sha256"),
        "importer_sha256": sha256(Path(__file__)),
        "excluded_engines": sorted(excluded_engines),
        "imported_case_count": len(entries),
        "cases": entries,
    }
    dest.mkdir(parents=True, exist_ok=True)
    out = dest / "import-manifest.json"
    out.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    manifest["manifest_sha256"] = sha256(out)
    return manifest


def main() -> int:
    p = argparse.ArgumentParser(description="Import accepted non-Persy concurrency v1 cases into a v2 run with explicit provenance")
    p.add_argument("source", type=Path)
    p.add_argument("dest", type=Path)
    p.add_argument("--exclude-engine", action="append", default=["persy"])
    args = p.parse_args()
    manifest = import_results(args.source, args.dest, set(args.exclude_engine))
    print(json.dumps({k: v for k, v in manifest.items() if k != "cases"}, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
