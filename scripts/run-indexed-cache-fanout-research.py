#!/usr/bin/env -S uv run --script
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import math
import os
import random
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

ENGINES = ("sqlite", "turso")
DEFAULT_LIMITS = (1, 5, 10, 25, 50, 100)
DEFAULT_CACHE_KIB: tuple[int | None, ...] = (None, 2000, 8000)
ADMISSION_POLICY = "pre-io+pre/post-external-v2"
READ_MATERIALIZATION = "full-record-v1"
WRITE_MATERIALIZATION = "no-return-v1"


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def io_full_avg10() -> float:
    for line in Path("/proc/pressure/io").read_text().splitlines():
        if line.startswith("full "):
            for part in line.split():
                if part.startswith("avg10="):
                    return float(part.split("=", 1)[1])
    raise RuntimeError("missing I/O full avg10")


def external_quiet(repo: Path, *, json_out: Path | None = None) -> bool:
    cmd = [sys.executable, str(repo / "scripts" / "check-external-noise.py"), "--sample-ms", "250"]
    if json_out is not None:
        json_out.parent.mkdir(parents=True, exist_ok=True)
        cmd += ["--json-out", str(json_out)]
    proc = subprocess.run(cmd, cwd=repo, check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return proc.returncode == 0


def preflight_quiet(repo: Path, *, max_io_full_avg10: float, evidence: Path | None = None) -> bool:
    if io_full_avg10() > max_io_full_avg10:
        return False
    return external_quiet(repo, json_out=evidence)


def wait_preflight(repo: Path, *, max_io_full_avg10: float, sleep_s: float) -> None:
    while not preflight_quiet(repo, max_io_full_avg10=max_io_full_avg10):
        time.sleep(max(sleep_s, 0.1))


def cache_slug(cache_kib: int | None) -> str:
    return "default" if cache_kib is None else f"{cache_kib}k"


def cell_slug(engine: str, cache_kib: int | None, limit: int) -> str:
    return f"{engine}-cache-{cache_slug(cache_kib)}-limit-{limit}"


def common_slug(cache_kib: int | None, limit: int) -> str:
    return f"cache-{cache_slug(cache_kib)}-limit-{limit}"


@dataclass(frozen=True)
class CaseResult:
    row: dict[str, Any]
    accepted_path: Path


def benchmark_command(
    *,
    bench_bin: Path,
    data_dir: Path,
    engine: str,
    cache_kib: int | None,
    limit: int,
    ops: int,
    trial: int,
    records: int,
    db_name: str,
    output: Path,
    scenario: str,
    reuse: bool,
) -> list[str]:
    cmd = [
        str(bench_bin),
        "--engine", engine,
        "--durability", "sync",
        "--workload", "indexed-read",
        "--records", str(records),
        "--ops", str(ops),
        "--payload-bytes", "512",
        "--txn-size", "100",
        "--indexed-read-limit", str(limit),
        "--trial", str(trial),
        "--seed", "1592606758",
        "--scenario", scenario,
        "--root", str(data_dir),
        "--db-name", db_name,
        "--warmup-reads", str(min(records, 10_000)),
        "--keep-db",
        "--output", str(output),
    ]
    if cache_kib is not None:
        cmd += ["--sql-cache-kib", str(cache_kib)]
    if reuse:
        cmd += ["--reuse-db", "--skip-prefill"]
    return cmd


def validate_row(
    row: dict[str, Any],
    *,
    engine: str,
    cache_kib: int | None,
    limit: int,
    ops: int,
    trial: int,
) -> None:
    expected = {
        "engine": engine,
        "durability": "sync",
        "workload": "indexed-read",
        "indexed_read_limit": limit,
        "sql_cache_kib": cache_kib,
        "ops_requested": ops,
        "ops_completed": ops,
        "trial": trial,
        "read_materialization": READ_MATERIALIZATION,
        "write_materialization": WRITE_MATERIALIZATION,
    }
    wrong = {key: (row.get(key), value) for key, value in expected.items() if row.get(key) != value}
    if wrong:
        raise RuntimeError(f"result identity mismatch: {wrong}")
    pragma_value = row.get("sql_cache_pragma_value")
    if cache_kib is not None and pragma_value != -cache_kib:
        raise RuntimeError(
            f"benchmark cache PRAGMA mismatch: expected {-cache_kib}, got {pragma_value!r}"
        )
    if cache_kib is None and not isinstance(pragma_value, int):
        raise RuntimeError(f"benchmark default cache PRAGMA is missing: {pragma_value!r}")


def run_attempt(
    *,
    repo: Path,
    bench_bin: Path,
    data_dir: Path,
    engine: str,
    cache_kib: int | None,
    limit: int,
    ops: int,
    trial: int,
    records: int,
    db_name: str,
    attempt_dir: Path,
    scenario: str,
    reuse: bool,
    max_io_full_avg10: float,
    sleep_s: float,
) -> dict[str, Any]:
    attempt = 0
    while True:
        attempt += 1
        evidence = attempt_dir / f"attempt-{attempt:03d}"
        evidence.mkdir(parents=True, exist_ok=True)
        wait_preflight(repo, max_io_full_avg10=max_io_full_avg10, sleep_s=sleep_s)
        before = evidence / "noise-before.json"
        if not preflight_quiet(repo, max_io_full_avg10=max_io_full_avg10, evidence=before):
            continue
        out = evidence / "result.ndjson"
        err = evidence / "stderr.log"
        cmd = benchmark_command(
            bench_bin=bench_bin,
            data_dir=data_dir,
            engine=engine,
            cache_kib=cache_kib,
            limit=limit,
            ops=ops,
            trial=trial,
            records=records,
            db_name=db_name,
            output=out,
            scenario=scenario,
            reuse=reuse,
        )
        with err.open("w") as stderr:
            proc = subprocess.run(cmd, cwd=repo, check=False, stderr=stderr)
        # Admission v2 deliberately does NOT inspect post-case I/O PSI.
        after = evidence / "noise-after.json"
        after_quiet = external_quiet(repo, json_out=after)
        if proc.returncode != 0:
            raise RuntimeError(f"benchmark failed rc={proc.returncode}: {err}")
        lines = [line for line in out.read_text().splitlines() if line.strip()]
        if len(lines) != 1:
            raise RuntimeError(f"expected one result row, got {len(lines)}: {out}")
        row = json.loads(lines[0])
        validate_row(row, engine=engine, cache_kib=cache_kib, limit=limit, ops=ops, trial=trial)
        if not after_quiet:
            (evidence / "rejected.json").write_text(
                json.dumps({"reason": "post-external-userspace", "attempt": attempt}, indent=2) + "\n"
            )
            continue
        return row


def prepare_engine(
    *,
    repo: Path,
    bench_bin: Path,
    data_dir: Path,
    evidence_root: Path,
    engine: str,
    records: int,
) -> str:
    db_name = f"prepared-{engine}-n{records}"
    db_path = data_dir / db_name
    marker = evidence_root / f"{engine}.json"
    if db_path.exists() and marker.exists():
        return db_name
    if db_path.exists() != marker.exists():
        raise RuntimeError(f"prepared DB/marker disagree for {engine}")
    marker.parent.mkdir(parents=True, exist_ok=True)
    # Preparation is outside performance admission. It exists only to build the immutable input DB.
    out = marker.with_suffix(".ndjson")
    cmd = benchmark_command(
        bench_bin=bench_bin,
        data_dir=data_dir,
        engine=engine,
        cache_kib=None,
        limit=1,
        ops=1,
        trial=0,
        records=records,
        db_name=db_name,
        output=out,
        scenario="indexed-cache-fanout-prepare",
        reuse=False,
    )
    proc = subprocess.run(cmd, cwd=repo, check=False, capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(f"prepare failed for {engine}: {proc.stderr}")
    row = json.loads(out.read_text().strip())
    validate_row(row, engine=engine, cache_kib=None, limit=1, ops=1, trial=0)
    marker.write_text(json.dumps({"db_name": db_name, "db_bytes": row["db_bytes"]}, indent=2) + "\n")
    return db_name


def calibrate_rate(
    *,
    repo: Path,
    bench_bin: Path,
    data_dir: Path,
    calibration_root: Path,
    engine: str,
    cache_kib: int | None,
    limit: int,
    records: int,
    db_name: str,
    minimum_probe_s: float,
    max_probe_ops: int,
    max_io_full_avg10: float,
    sleep_s: float,
) -> float:
    meta = calibration_root / f"{cell_slug(engine, cache_kib, limit)}.json"
    if meta.exists():
        saved = json.loads(meta.read_text())
        expected = {
            "engine": engine,
            "sql_cache_kib": cache_kib,
            "indexed_read_limit": limit,
        }
        wrong = {key: (saved.get(key), value) for key, value in expected.items() if saved.get(key) != value}
        if wrong:
            raise RuntimeError(f"calibration identity mismatch for {meta}: {wrong}")
        rate = float(saved.get("ops_per_s", 0.0))
        if not rate > 0.0:
            raise RuntimeError(f"invalid cached calibration rate in {meta}: {rate}")
        return rate
    ops = 1_000
    probe = 0
    while True:
        probe += 1
        row = run_attempt(
            repo=repo,
            bench_bin=bench_bin,
            data_dir=data_dir,
            engine=engine,
            cache_kib=cache_kib,
            limit=limit,
            ops=ops,
            trial=0,
            records=records,
            db_name=db_name,
            attempt_dir=calibration_root / "attempts" / cell_slug(engine, cache_kib, limit) / f"probe-{probe:02d}",
            scenario="indexed-cache-fanout-calibration",
            reuse=True,
            max_io_full_avg10=max_io_full_avg10,
            sleep_s=sleep_s,
        )
        elapsed = float(row["elapsed_s"])
        if elapsed >= minimum_probe_s or ops >= max_probe_ops:
            rate = float(row["ops_per_s"])
            meta.parent.mkdir(parents=True, exist_ok=True)
            meta.write_text(json.dumps({
                "engine": engine,
                "sql_cache_kib": cache_kib,
                "indexed_read_limit": limit,
                "probe_ops": ops,
                "elapsed_s": elapsed,
                "ops_per_s": rate,
            }, indent=2, sort_keys=True) + "\n")
            return rate
        ops = min(max_probe_ops, ops * 2)


def choose_common_ops(rates: dict[str, float], *, target_s: float, ceiling_s: float) -> int:
    fastest = max(rates.values())
    slowest = min(rates.values())
    raw = min(fastest * target_s, slowest * ceiling_s)
    quantum = 100 if raw < 100_000 else 1_000
    return max(100, math.floor(raw / quantum) * quantum)


def main() -> int:
    ap = argparse.ArgumentParser(description="SQLite/Turso indexed-read fanout and cache-budget diagnostic")
    ap.add_argument("--repo", type=Path, required=True)
    ap.add_argument("--bench-bin", type=Path, required=True)
    ap.add_argument("--run-id", default="record-cache-fanout")
    ap.add_argument("--records", type=int, default=10_000)
    ap.add_argument("--limit", type=int, action="append", dest="limits")
    ap.add_argument("--cache-kib", action="append", dest="cache_kib", help="integer KiB or 'default'")
    ap.add_argument("--trials", type=int, default=5)
    ap.add_argument("--target-s", type=float, default=3.0)
    ap.add_argument("--slowest-ceiling-s", type=float, default=15.0)
    ap.add_argument("--minimum-probe-s", type=float, default=0.5)
    ap.add_argument("--max-probe-ops", type=int, default=2_000_000)
    ap.add_argument("--max-io-full-avg10", type=float, default=5.0)
    ap.add_argument("--busy-sleep", type=float, default=1.0)
    ap.add_argument("--lock-file", type=Path, default=Path("/run/lock/all-db-bench-performance.lock"))
    args = ap.parse_args()

    repo = args.repo.resolve()
    bench_bin = args.bench_bin.resolve()
    limits = tuple(args.limits or DEFAULT_LIMITS)
    caches: tuple[int | None, ...]
    if args.cache_kib:
        parsed: list[int | None] = []
        for raw in args.cache_kib:
            if raw == "default":
                parsed.append(None)
            else:
                value = int(raw)
                if value <= 0:
                    ap.error("--cache-kib must be positive or 'default'")
                parsed.append(value)
        caches = tuple(parsed)
    else:
        caches = DEFAULT_CACHE_KIB

    if args.records < 100 or args.records % 100:
        ap.error("--records must be >=100 and divisible by 100")
    bucket_rows = args.records // 100
    if any(limit <= 0 or limit > bucket_rows for limit in limits):
        ap.error(f"each --limit must be in 1..{bucket_rows}")
    if args.trials < 3:
        ap.error("--trials must be >=3")
    if not bench_bin.is_file() or not os.access(bench_bin, os.X_OK):
        ap.error(f"benchmark binary is not executable: {bench_bin}")

    args.lock_file.parent.mkdir(parents=True, exist_ok=True)
    lock = args.lock_file.open("a+")
    try:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        print(f"performance lock busy: {args.lock_file}", file=sys.stderr)
        return 73

    root = args.state_root.resolve() / args.run_id
    data_dir = root / "data"
    results_dir = root / "results"
    for path in (data_dir, results_dir):
        path.mkdir(parents=True, exist_ok=True)

    binary_sha = sha256(bench_bin)
    noise_sha = sha256(repo / "scripts/check-external-noise.py")
    support = {
        "kind": "indexed-cache-fanout-v1",
        "run_id": args.run_id,
        "state_root": str(args.state_root.resolve()),
        "records": args.records,
        "canonical_comparison": args.records == 10_000 and tuple(limits) == DEFAULT_LIMITS,
        "limits": list(limits),
        "cache_kib": list(caches),
        "trials": args.trials,
        "target_s": args.target_s,
        "slowest_ceiling_s": args.slowest_ceiling_s,
        "minimum_probe_s": args.minimum_probe_s,
        "max_io_full_avg10": args.max_io_full_avg10,
        "engines": list(ENGINES),
        "durability": "sync",
        "benchmark_binary": str(bench_bin),
        "benchmark_binary_sha256": binary_sha,
        "repo_head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
        "noise_guard_sha256": noise_sha,
        "admission_policy": ADMISSION_POLICY,
        "read_materialization": READ_MATERIALIZATION,
        "write_materialization": WRITE_MATERIALIZATION,
        "default_cache_context": {
            "sqlite": "SQLite 3.53.4 cache_size=-2000 (2000 KiB by SQLite negative-cache-size semantics)",
            "turso": "turso_core 0.8.2 constructs the pager with PageCache::default()=2000 pages; the connection header also reports cache_size=-2000, but the pager is resized only when PRAGMA cache_size is executed",
            "equalized_cache_kib": [2000, 8000],
        },
        "method": "prepared DB; accepted calibration; common final work per cache×fanout; pre-I/O + pre/post userspace admission; default matrix reproduces canonical 10k-record indexed-read geometry",
    }
    support_path = results_dir / "support.json"
    encoded = json.dumps(support, indent=2, sort_keys=True) + "\n"
    if support_path.exists() and support_path.read_text() != encoded:
        raise RuntimeError("refusing resume: support identity changed")
    support_path.write_text(encoded)

    db_names = {
        engine: prepare_engine(
            repo=repo,
            bench_bin=bench_bin,
            data_dir=data_dir,
            evidence_root=root / "prepare",
            engine=engine,
            records=args.records,
        )
        for engine in ENGINES
    }
    wait_preflight(repo, max_io_full_avg10=args.max_io_full_avg10, sleep_s=args.busy_sleep)

    calibration_root = root / "calibration"
    calibration_root.mkdir(parents=True, exist_ok=True)
    common_ops: dict[tuple[int | None, int], int] = {}
    for cache_kib in caches:
        for limit in limits:
            rates = {
                engine: calibrate_rate(
                    repo=repo,
                    bench_bin=bench_bin,
                    data_dir=data_dir,
                    calibration_root=calibration_root,
                    engine=engine,
                    cache_kib=cache_kib,
                    limit=limit,
                    records=args.records,
                    db_name=db_names[engine],
                    minimum_probe_s=args.minimum_probe_s,
                    max_probe_ops=args.max_probe_ops,
                    max_io_full_avg10=args.max_io_full_avg10,
                    sleep_s=args.busy_sleep,
                )
                for engine in ENGINES
            }
            ops = choose_common_ops(rates, target_s=args.target_s, ceiling_s=args.slowest_ceiling_s)
            common_ops[(cache_kib, limit)] = ops
            (calibration_root / f"{common_slug(cache_kib, limit)}.common.json").write_text(
                json.dumps({"rates": rates, "selected_ops": ops}, indent=2, sort_keys=True) + "\n"
            )

    cases = [
        (cache_kib, limit, engine, trial)
        for cache_kib in caches
        for limit in limits
        for engine in ENGINES
        for trial in range(1, args.trials + 1)
    ]
    random.Random(0xCA_C4E_F00D).shuffle(cases)
    accepted_dir = results_dir / "cases"
    accepted_dir.mkdir(parents=True, exist_ok=True)
    for index, (cache_kib, limit, engine, trial) in enumerate(cases, 1):
        case_id = f"{cell_slug(engine, cache_kib, limit)}-t{trial}"
        accepted = accepted_dir / f"{case_id}.json"
        ops = common_ops[(cache_kib, limit)]
        if accepted.exists():
            row = json.loads(accepted.read_text())
            validate_row(
                row,
                engine=engine,
                cache_kib=cache_kib,
                limit=limit,
                ops=ops,
                trial=trial,
            )
            continue
        row = run_attempt(
            repo=repo,
            bench_bin=bench_bin,
            data_dir=data_dir,
            engine=engine,
            cache_kib=cache_kib,
            limit=limit,
            ops=ops,
            trial=trial,
            records=args.records,
            db_name=db_names[engine],
            attempt_dir=root / "attempts" / case_id,
            scenario="indexed-cache-fanout",
            reuse=True,
            max_io_full_avg10=args.max_io_full_avg10,
            sleep_s=args.busy_sleep,
         )
        tmp = accepted.with_suffix(".tmp")
        tmp.write_text(json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n")
        os.replace(tmp, accepted)
        print(f"[{index}/{len(cases)}] {case_id} ops={ops} rate={float(row['ops_per_s']):.1f}", flush=True)

    rows = []
    for path in sorted(accepted_dir.glob("*.json")):
        rows.append(json.loads(path.read_text()))
    expected = len(cases)
    if len(rows) != expected:
        raise RuntimeError(f"incomplete final corpus: {len(rows)}/{expected}")
    ndjson = results_dir / "results.ndjson"
    ndjson.write_text("".join(json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n" for row in rows))
    summary_json = results_dir / "summary.json"
    summary_md = results_dir / "summary.md"
    subprocess.run(
        [
            sys.executable,
            str(repo / "scripts" / "summarize.py"),
            str(ndjson),
            "--json-out", str(summary_json),
            "--markdown-out", str(summary_md),
            "--expect-trials", str(args.trials),
        ],
        cwd=repo,
        check=True,
    )
    summary = json.loads(summary_json.read_text())
    if summary.get("problems") or int(summary.get("row_count", -1)) != expected:
        raise RuntimeError("diagnostic summary validation failed")
    print(json.dumps({"complete": True, "rows": len(rows), "results": str(ndjson), "summary": str(summary_json)}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
