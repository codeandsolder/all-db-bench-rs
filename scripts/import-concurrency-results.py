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


def verify_import(run_dir: Path) -> dict[str, Any]:
    manifest_path = run_dir / "import-manifest.json"
    if not manifest_path.is_file():
        raise RuntimeError(f"missing import manifest: {manifest_path}")
    manifest = load_json(manifest_path)
    expected_importer = manifest.get("importer_sha256")
    actual_importer = sha256(Path(__file__))
    if expected_importer != actual_importer:
        raise RuntimeError(
            f"importer SHA-256 mismatch: manifest={expected_importer} current={actual_importer}"
        )
    entries = manifest.get("cases")
    if not isinstance(entries, list):
        raise RuntimeError("import manifest cases must be a list")
    if manifest.get("imported_case_count") != len(entries):
        raise RuntimeError("imported_case_count does not match manifest case list")
    excluded = set(manifest.get("excluded_engines", []))
    seen: set[str] = set()
    for entry in entries:
        case_id = entry.get("case_id")
        engine = entry.get("engine")
        if not isinstance(case_id, str) or not case_id or case_id in seen:
            raise RuntimeError(f"invalid or duplicate imported case_id: {case_id!r}")
        seen.add(case_id)
        if engine in excluded:
            raise RuntimeError(f"excluded engine appears in imported cases: {engine}")
        paths = {
            "result_sha256": run_dir / "cases" / f"{case_id}.json",
            "noise_before_sha256": run_dir / "noise" / f"{case_id}.before.json",
            "noise_after_sha256": run_dir / "noise" / f"{case_id}.after.json",
        }
        for field, path in paths.items():
            if not path.is_file():
                raise RuntimeError(f"missing imported artifact: {path}")
            actual = sha256(path)
            if actual != entry.get(field):
                raise RuntimeError(
                    f"imported artifact SHA-256 mismatch for {case_id} {field}: "
                    f"manifest={entry.get(field)} actual={actual}"
                )
        require_quiet(paths["noise_before_sha256"])
        require_quiet(paths["noise_after_sha256"])
    return manifest


def main() -> int:
    p = argparse.ArgumentParser(description="Import or verify accepted concurrency results with explicit provenance")
    p.add_argument("source", type=Path, nargs="?")
    p.add_argument("dest", type=Path, nargs="?")
    p.add_argument("--exclude-engine", action="append", default=["persy"])
    p.add_argument("--verify-run", type=Path)
    args = p.parse_args()
    if args.verify_run is not None:
        if args.source is not None or args.dest is not None:
            p.error("--verify-run cannot be combined with source/dest")
        manifest = verify_import(args.verify_run)
        print(json.dumps({"verified": True, "imported_case_count": manifest["imported_case_count"]}, sort_keys=True))
        return 0
    if args.source is None or args.dest is None:
        p.error("source and dest are required unless --verify-run is used")
    manifest = import_results(args.source, args.dest, set(args.exclude_engine))
    print(json.dumps({k: v for k, v in manifest.items() if k != "cases"}, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
