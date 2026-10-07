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

BASE_REQUIRED = ("scenario", "engine", "durability", "workload", "clients", "records", "ops", "trial")
STATE_FIELDS = ("state_evolution", "write_pattern", "bounded_churn_slots")


def materialize(plan: dict[str, Any]) -> tuple[list[list[str]], dict[str, int]]:
    version = int(plan.get("concurrency_probe_plan_version", -1))
    if version not in (1, 2):
        raise ValueError(f"unsupported plan version: {version!r}")
    cases = plan.get("cases")
    if not isinstance(cases, list) or len(cases) != int(plan.get("case_count", -1)):
        raise ValueError("plan case_count mismatch")
    expect_trials = int(plan.get("expect_trials", 1))
    if expect_trials < 1:
        raise ValueError("expect_trials must be positive")

    seen: set[tuple[Any, ...]] = set()
    families: dict[tuple[Any, ...], tuple[int, int]] = {}
    client_trials: dict[tuple[Any, ...], set[int]] = defaultdict(set)
    materialized: list[list[str]] = []
    for i, item in enumerate(cases):
        missing = [key for key in BASE_REQUIRED if key not in item]
        if missing:
            raise ValueError(f"plan case {i} missing {missing}")
        state = str(item.get("state_evolution", "growth"))
        write = str(item.get("write_pattern", "append"))
        slots = int(item.get("bounded_churn_slots", 0))
        if version == 2:
            missing = [key for key in STATE_FIELDS if key not in item]
            if missing:
                raise ValueError(f"v2 plan case {i} missing {missing}")
        if state not in {"growth", "bounded"}:
            raise ValueError(f"invalid state_evolution in case {i}: {state}")
        if write not in {"append", "update-uniform", "update-hot"}:
            raise ValueError(f"invalid write_pattern in case {i}: {write}")
        if slots < 0:
            raise ValueError(f"negative bounded_churn_slots in case {i}")
        if state == "growth" and slots != 0:
            raise ValueError(f"growth case {i} must use bounded_churn_slots=0")
        clients = int(item["clients"])
        records = int(item["records"])
        ops = int(item["ops"])
        trial = int(item["trial"])
        if min(clients, records, ops, trial) < 1:
            raise ValueError(f"non-positive numeric field in case {i}")
        if clients > ops:
            raise ValueError(f"clients > ops in case {i}")
        workload = str(item["workload"])
        if workload == "delete-burst" and ops > records:
            raise ValueError(f"delete-burst ops > records in case {i}")
        if state == "bounded":
            if workload == "delete-burst":
                raise ValueError(f"bounded delete-burst unsupported in case {i}")
            if workload in {"tiny-txn", "write-burst"} and write == "append":
                raise ValueError(f"bounded {workload} requires update write pattern in case {i}")
            if workload in {"balanced", "churn"}:
                if write != "update-uniform":
                    raise ValueError(f"bounded {workload} requires canonical update-uniform in case {i}")
                if slots < clients * 300:
                    raise ValueError(f"bounded_churn_slots too small for client count in case {i}")
        key = (
            item["scenario"], item["engine"], item["durability"], workload,
            state, write, slots, clients, trial,
        )
        if key in seen:
            raise ValueError(f"duplicate case identity: {key}")
        seen.add(key)
        client_key = key[:-1]
        client_trials[client_key].add(trial)
        family = key[:-2]
        previous = families.setdefault(family, (records, ops))
        if previous != (records, ops):
            raise ValueError(f"family total-work mismatch: {family}: {previous} != {(records, ops)}")
        vals = [str(item[k]) for k in BASE_REQUIRED] + [state, write, str(slots)]
        if any("|" in value or "\n" in value for value in vals):
            raise ValueError(f"invalid delimiter in case {i}")
        materialized.append(vals)

    for client_key, trials in client_trials.items():
        if len(trials) != expect_trials:
            raise ValueError(f"client group {client_key} has {len(trials)} trials, expected {expect_trials}")
    if materialized and len(materialized) % expect_trials != 0:
        raise ValueError("plan case count is not divisible by expect_trials")
    return materialized, {"plan_version": version, "expect_trials": expect_trials, "case_count": len(materialized)}


def main() -> int:
    parser = argparse.ArgumentParser(description="Validate and materialize an exact KV concurrency plan")
    parser.add_argument("plan", type=Path)
    parser.add_argument("tsv_out", type=Path)
    parser.add_argument("meta_out", type=Path)
    args = parser.parse_args()
    try:
        rows, meta = materialize(json.loads(args.plan.read_text()))
    except (OSError, json.JSONDecodeError, TypeError, ValueError) as error:
        raise SystemExit(str(error)) from error
    with args.tsv_out.open("w") as out:
        for values in rows:
            out.write("|".join(values) + "\n")
    args.meta_out.write_text(json.dumps(meta, sort_keys=True, separators=(",", ":")) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
