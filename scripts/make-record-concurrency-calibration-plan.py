#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
import subprocess
from functools import lru_cache
from pathlib import Path
from typing import Any

ENGINES = ("surrealdb", "turso", "sqlite", "surrealdb-rocksdb")
CORE_CLIENTS = (1, 2, 4, 8)
SIDE_CLIENTS = (1, 4, 8)
RECORDS = 10_000
PAYLOAD = 512
TXN = 100
DEFAULT_OPS = 24_000
STRESS_PAYLOAD = 4_096
HOT_RECORDS = 64
HOT_DEFAULT_OPS = 24_000
TX1_DEFAULT_OPS = 24_000
TX1000_DEFAULT_OPS = 24_000
BOUNDED_WORKLOADS = {"tiny-txn", "write-burst"}


@lru_cache(maxsize=None)
def policy_ops(repo: str, scenario: str, workload: str, default_ops: int) -> int:
    policy = Path(repo) / "scripts" / "concurrency-matrix-policy.sh"
    script = (
        f"source {policy!s}; "
        f"record_concurrency_ops quick {scenario} {workload} {default_ops}"
    )
    proc = subprocess.run(
        ["bash", "-lc", script],
        cwd=repo,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if proc.returncode != 0:
        raise ValueError(
            f"record_concurrency_ops failed for {scenario}/{workload}: "
            f"rc={proc.returncode}: {proc.stderr.strip()}"
        )
    try:
        value = int(proc.stdout.strip())
    except ValueError as error:
        raise ValueError(f"invalid policy op count: {proc.stdout!r}") from error
    if value <= 0:
        raise ValueError(f"non-positive policy op count: {value}")
    return value


def add_case(
    cases: list[dict[str, Any]],
    *,
    repo: str,
    scenario: str,
    engine: str,
    durability: str,
    workload: str,
    clients: int,
    records: int,
    default_ops: int,
    payload_bytes: int,
    txn_size: int,
) -> None:
    ops = policy_ops(repo, scenario, workload, default_ops)
    state = "bounded" if workload in BOUNDED_WORKLOADS else "growth"
    cases.append(
        {
            "scenario": scenario,
            "engine": engine,
            "durability": durability,
            "workload": workload,
            "clients": clients,
            "records": records,
            "ops": ops,
            "payload_bytes": payload_bytes,
            "txn_size": txn_size,
            "trial": 1,
            "state_evolution": state,
        }
    )


def build_plan(repo: Path) -> dict[str, Any]:
    repo_s = str(repo)
    cases: list[dict[str, Any]] = []
    for engine in ENGINES:
        for clients in CORE_CLIENTS:
            for workload in ("point-read", "indexed-read", "read-heavy", "tiny-txn", "write-burst"):
                add_case(
                    cases,
                    repo=repo_s,
                    scenario="record-concurrency-core",
                    engine=engine,
                    durability="sync",
                    workload=workload,
                    clients=clients,
                    records=RECORDS,
                    default_ops=DEFAULT_OPS,
                    payload_bytes=PAYLOAD,
                    txn_size=TXN,
                )
        for clients in SIDE_CLIENTS:
            for workload in ("read-heavy", "write-burst"):
                add_case(
                    cases,
                    repo=repo_s,
                    scenario="record-concurrency-relaxed",
                    engine=engine,
                    durability="relaxed",
                    workload=workload,
                    clients=clients,
                    records=RECORDS,
                    default_ops=DEFAULT_OPS,
                    payload_bytes=PAYLOAD,
                    txn_size=TXN,
                )
        for clients in SIDE_CLIENTS:
            for workload in ("read-heavy", "write-burst"):
                add_case(
                    cases,
                    repo=repo_s,
                    scenario="record-concurrency-large-payload",
                    engine=engine,
                    durability="sync",
                    workload=workload,
                    clients=clients,
                    records=RECORDS,
                    default_ops=DEFAULT_OPS,
                    payload_bytes=STRESS_PAYLOAD,
                    txn_size=TXN,
                )
        for clients in SIDE_CLIENTS:
            add_case(
                cases,
                repo=repo_s,
                scenario="record-concurrency-hotset",
                engine=engine,
                durability="sync",
                workload="read-heavy",
                clients=clients,
                records=HOT_RECORDS,
                default_ops=HOT_DEFAULT_OPS,
                payload_bytes=PAYLOAD,
                txn_size=TXN,
            )
        for clients in SIDE_CLIENTS:
            add_case(
                cases,
                repo=repo_s,
                scenario="record-concurrency-txn-1",
                engine=engine,
                durability="sync",
                workload="write-burst",
                clients=clients,
                records=RECORDS,
                default_ops=TX1_DEFAULT_OPS,
                payload_bytes=PAYLOAD,
                txn_size=1,
            )
            add_case(
                cases,
                repo=repo_s,
                scenario="record-concurrency-txn-1000",
                engine=engine,
                durability="sync",
                workload="write-burst",
                clients=clients,
                records=RECORDS,
                default_ops=TX1000_DEFAULT_OPS,
                payload_bytes=PAYLOAD,
                txn_size=1000,
            )

    return {
        "record_concurrency_plan_version": 1,
        "kind": "semantic-calibration",
        "expect_trials": 1,
        "case_count": len(cases),
        "profile": "quick",
        "cases": cases,
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Generate one-shot semantic calibration for record concurrency"
    )
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    plan = build_plan(args.repo)
    if plan["case_count"] != 164:
        raise SystemExit(f"unexpected record calibration cardinality: {plan['case_count']} != 164")
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(plan, indent=2, sort_keys=True) + "\n")
    print(json.dumps({"case_count": plan["case_count"], "expect_trials": 1}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
