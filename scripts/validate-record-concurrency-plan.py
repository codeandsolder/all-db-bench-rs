#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
from collections import defaultdict
from pathlib import Path
from typing import Any

REQUIRED = (
    "scenario", "engine", "durability", "workload", "clients", "records",
    "ops", "payload_bytes", "txn_size", "trial", "state_evolution", "read_materialization",
    "write_materialization",
)
ALLOWED_ENGINES = {"surrealdb", "turso", "sqlite", "surrealdb-rocksdb"}
ALLOWED_DURABILITY = {"sync", "relaxed"}
ALLOWED_WORKLOADS = {"point-read", "indexed-read", "read-heavy", "tiny-txn", "write-burst"}
ALLOWED_STATE = {"growth", "bounded"}
PLAN_VERSION = 2
READ_MATERIALIZATION = "full-record-v1"
WRITE_MATERIALIZATION = "no-return-v1"


def family_key(item: dict[str, Any]) -> tuple[Any, ...]:
    return (
        item["scenario"], item["engine"], item["durability"], item["workload"],
        int(item["payload_bytes"]), int(item["txn_size"]), item["state_evolution"],
        item["read_materialization"], item["write_materialization"],
    )


def client_key(item: dict[str, Any]) -> tuple[Any, ...]:
    return family_key(item) + (int(item["clients"]),)


def materialize(plan: dict[str, Any]) -> tuple[list[list[str]], dict[str, Any]]:
    version = int(plan.get("record_concurrency_plan_version", -1))
    if version != PLAN_VERSION:
        raise ValueError(f"unsupported record concurrency plan version: {version}")
    if plan.get("read_materialization") != READ_MATERIALIZATION:
        raise ValueError(f"invalid plan read_materialization: {plan.get('read_materialization')!r}")
    if plan.get("write_materialization") != WRITE_MATERIALIZATION:
        raise ValueError(f"invalid plan write_materialization: {plan.get('write_materialization')!r}")
    cases = plan.get("cases")
    if not isinstance(cases, list) or len(cases) != int(plan.get("case_count", -1)):
        raise ValueError("plan case_count mismatch")
    expect_trials = int(plan.get("expect_trials", 0))
    if expect_trials < 1:
        raise ValueError("expect_trials must be positive")

    seen: set[tuple[Any, ...]] = set()
    families: dict[tuple[Any, ...], tuple[int, int]] = {}
    trials_by_client: dict[tuple[Any, ...], set[int]] = defaultdict(set)
    rows: list[list[str]] = []

    for i, item in enumerate(cases):
        missing = [field for field in REQUIRED if field not in item]
        if missing:
            raise ValueError(f"plan case {i} missing {missing}")
        if item["engine"] not in ALLOWED_ENGINES:
            raise ValueError(f"invalid engine in case {i}: {item['engine']}")
        if item["durability"] not in ALLOWED_DURABILITY:
            raise ValueError(f"invalid durability in case {i}: {item['durability']}")
        if item["workload"] not in ALLOWED_WORKLOADS:
            raise ValueError(f"invalid workload in case {i}: {item['workload']}")
        if item["state_evolution"] not in ALLOWED_STATE:
            raise ValueError(f"invalid state_evolution in case {i}: {item['state_evolution']}")
        if item["read_materialization"] != READ_MATERIALIZATION:
            raise ValueError(f"invalid read_materialization in case {i}: {item['read_materialization']!r}")
        if item["write_materialization"] != WRITE_MATERIALIZATION:
            raise ValueError(f"invalid write_materialization in case {i}: {item['write_materialization']!r}")

        clients = int(item["clients"])
        records = int(item["records"])
        ops = int(item["ops"])
        payload = int(item["payload_bytes"])
        txn = int(item["txn_size"])
        trial = int(item["trial"])
        if min(clients, records, ops, payload, txn, trial) < 1:
            raise ValueError(f"non-positive numeric field in case {i}")
        if clients > ops:
            raise ValueError(f"clients > ops in case {i}")
        workload = item["workload"]
        state = item["state_evolution"]
        if state == "bounded" and workload not in {"tiny-txn", "write-burst"}:
            raise ValueError(f"bounded state unsupported for {workload} in case {i}")
        if workload == "write-burst":
            if ops % txn:
                raise ValueError(f"write-burst ops must be divisible by txn_size in case {i}")
            if ops < clients * txn:
                raise ValueError(f"write-burst must provide a whole txn per client in case {i}")
            if state == "bounded" and txn > records:
                raise ValueError(f"bounded write-burst txn_size > records in case {i}")

        identity = client_key(item) + (trial,)
        if identity in seen:
            raise ValueError(f"duplicate case identity: {identity}")
        seen.add(identity)
        trials_by_client[client_key(item)].add(trial)
        fk = family_key(item)
        prior = families.setdefault(fk, (records, ops))
        if prior != (records, ops):
            raise ValueError(f"family total-work mismatch: {fk}: {prior} != {(records, ops)}")

        values = [
            str(item["scenario"]), str(item["engine"]), str(item["durability"]),
            str(workload), str(clients), str(records), str(ops), str(payload),
            str(txn), str(trial), str(state), str(item["read_materialization"]),
            str(item["write_materialization"]),
        ]
        if any("|" in value or "\n" in value for value in values):
            raise ValueError(f"invalid delimiter in case {i}")
        rows.append(values)

    for ck, trials in trials_by_client.items():
        if trials != set(range(1, expect_trials + 1)):
            raise ValueError(
                f"client group {ck} has trials {sorted(trials)}, "
                f"expected {list(range(1, expect_trials + 1))}"
            )
    if rows and len(rows) % expect_trials:
        raise ValueError("case count is not divisible by expect_trials")
    return rows, {
        "plan_version": version,
        "expect_trials": expect_trials,
        "case_count": len(rows),
        "kind": str(plan.get("kind", "record-concurrency-plan")),
        "read_materialization": READ_MATERIALIZATION,
        "write_materialization": WRITE_MATERIALIZATION,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="Validate and materialize record concurrency plan")
    parser.add_argument("plan", type=Path)
    parser.add_argument("tsv_out", type=Path, nargs="?")
    parser.add_argument("meta_out", type=Path, nargs="?")
    args = parser.parse_args()
    try:
        rows, meta = materialize(json.loads(args.plan.read_text()))
    except (OSError, json.JSONDecodeError, TypeError, ValueError) as error:
        raise SystemExit(str(error)) from error
    if args.tsv_out:
        args.tsv_out.write_text("".join("|".join(row) + "\n" for row in rows))
    if args.meta_out:
        args.meta_out.write_text(json.dumps(meta, sort_keys=True, separators=(",", ":")) + "\n")
    if not args.tsv_out and not args.meta_out:
        print(json.dumps(meta, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
