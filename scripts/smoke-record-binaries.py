#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import tempfile
from pathlib import Path

READ = "full-record-v1"
WRITE = "no-return-v1"


def checked_result(cmd: list[str], output: Path, *, label: str, expected_format: int, expected_ops: int) -> None:
    proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, check=False)
    if proc.returncode != 0:
        raise RuntimeError(f"runtime smoke failed for {label} rc={proc.returncode}\n{proc.stdout}")
    rows = [json.loads(line) for line in output.read_text().splitlines() if line.strip()]
    if len(rows) != 1:
        raise RuntimeError(f"expected one result row for {label}, got {len(rows)}")
    row = rows[0]
    if row.get("read_materialization") != READ or row.get("write_materialization") != WRITE:
        raise RuntimeError(f"semantic marker mismatch for {label}: {row}")
    if int(row.get("ops_completed", -1)) != expected_ops:
        raise RuntimeError(f"operation count mismatch for {label}: {row.get('ops_completed')}")
    if int(row.get("format_version", -1)) != expected_format:
        raise RuntimeError(f"format version mismatch for {label}: {row.get('format_version')}")
    print(f"record runtime smoke ok: {label}")


def run_baseline(binary: Path, engine: str, workload: str, root: Path) -> None:
    output = root / f"baseline-{engine}-{workload}.json"
    checked_result(
        [
            str(binary),
            "--engine", engine,
            "--durability", "sync",
            "--workload", workload,
            "--records", "32",
            "--ops", "64",
            "--payload-bytes", "64",
            "--txn-size", "8",
            "--warmup-reads", "8",
            "--root", str(root / f"baseline-{engine}-{workload}"),
            "--output", str(output),
        ],
        output,
        label=f"baseline/{engine}/{workload}",
        expected_format=7,
        expected_ops=64,
    )


def run_concurrency(binary: Path, engine: str, root: Path) -> None:
    output = root / f"concurrency-{engine}.json"
    checked_result(
        [
            str(binary),
            "--engine", engine,
            "--durability", "sync",
            "--workload", "point-read",
            "--state-evolution", "bounded",
            "--records", "32",
            "--ops", "64",
            "--clients", "2",
            "--payload-bytes", "64",
            "--txn-size", "8",
            "--warmup-reads", "8",
            "--root", str(root / f"concurrency-{engine}"),
            "--output", str(output),
        ],
        output,
        label=f"concurrency/{engine}/point-read-c2",
        expected_format=8,
        expected_ops=64,
    )


def run_sustained(binary: Path, engine: str, root: Path) -> None:
    output = root / f"sustained-{engine}.json"
    checked_result(
        [
            str(binary),
            "--engine", engine,
            "--durability", "sync",
            "--pattern", "churn",
            "--records", "32",
            "--ops", "40",
            "--window-ops", "20",
            "--payload-bytes", "64",
            "--txn-size", "10",
            "--warmup-reads", "8",
            "--settle-ms", "0",
            "--root", str(root / f"sustained-{engine}"),
            "--output", str(output),
        ],
        output,
        label=f"sustained/{engine}/churn",
        expected_format=7,
        expected_ops=40,
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Runtime smoke pinned record benchmark binaries against real engines"
    )
    parser.add_argument("--bench-bin", type=Path, required=True)
    parser.add_argument("--concurrency-bin", type=Path, required=True)
    parser.add_argument("--sustained-bin", type=Path, required=True)
    parser.add_argument("--rocks-bench-bin", type=Path, required=True)
    parser.add_argument("--rocks-concurrency-bin", type=Path, required=True)
    parser.add_argument("--rocks-sustained-bin", type=Path, required=True)
    args = parser.parse_args()
    for path in (
        args.bench_bin,
        args.concurrency_bin,
        args.sustained_bin,
        args.rocks_bench_bin,
        args.rocks_concurrency_bin,
        args.rocks_sustained_bin,
    ):
        if not path.is_file():
            parser.error(f"binary does not exist: {path}")

    root = Path(tempfile.mkdtemp(prefix="record-runtime-smoke-", dir="/tmp"))
    try:
        for engine in ("surrealdb", "turso", "sqlite"):
            for workload in ("point-read", "write-burst"):
                run_baseline(args.bench_bin, engine, workload, root)
        for workload in ("point-read", "write-burst"):
            run_baseline(args.rocks_bench_bin, "surrealdb-rocksdb", workload, root)

        run_concurrency(args.concurrency_bin, "surrealdb", root)
        run_concurrency(args.rocks_concurrency_bin, "surrealdb-rocksdb", root)

        for engine in ("surrealdb", "turso", "sqlite"):
            run_sustained(args.sustained_bin, engine, root)
        run_sustained(args.rocks_sustained_bin, "surrealdb-rocksdb", root)
    finally:
        shutil.rmtree(root, ignore_errors=True)
    print("record runtime smoke complete: 14/14")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
