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

IDENTITY_FIELDS = (
    "format_version", "lane", "scenario", "engine", "engine_version", "durability",
    "workload", "records", "ops_requested", "value_bytes", "value_pattern", "key_bytes",
    "key_shape", "access_pattern", "miss_percent", "write_pattern", "txn_size", "scan_len",
    "settle_ms",
)


def parse_clients(value: Any) -> list[int]:
    if isinstance(value, list):
        return [int(v) for v in value]
    return [int(v) for v in str(value).split() if str(v).strip()]


def expected_clients(support: dict[str, Any], scenario: str) -> list[int]:
    lane = support.get("lane")
    if lane == "kv-concurrency":
        key = {
            "concurrency-primary": "clients",
            "concurrency-range": "range_clients",
            "concurrency-delete": "delete_clients",
            "concurrency-relaxed": "relaxed_clients",
        }.get(scenario)
    elif lane == "record-concurrency":
        key = {
            "record-concurrency-core": "clients",
            "record-concurrency-relaxed": "relaxed_clients",
            "record-concurrency-large-payload": "stress_clients",
            "record-concurrency-hotset": "stress_clients",
            "record-concurrency-txn-1": "tx_clients",
            "record-concurrency-txn-1000": "tx_clients",
        }.get(scenario)
    else:
        raise ValueError(f"unsupported concurrency lane: {lane!r}")
    if key is None or key not in support:
        raise ValueError(f"no client policy for scenario {scenario!r} in lane {lane!r}")
    return parse_clients(support[key])


def identity(row: dict[str, Any]) -> tuple[Any, ...]:
    return tuple(row.get(field) for field in IDENTITY_FIELDS)


def rejected_pressure_attempts(run_dir: Path) -> int:
    root = run_dir / "rejected-pressure"
    return sum(1 for _ in root.glob("*/attempt-*/rejection.json")) if root.is_dir() else 0


def build_report(
    summary: dict[str, Any],
    support: dict[str, Any],
    *,
    run_dir: Path | None = None,
    allow_incomplete: bool = False,
) -> dict[str, Any]:
    lane = support.get("lane")
    if lane not in {"kv-concurrency", "record-concurrency"}:
        raise ValueError(f"unsupported concurrency lane: {lane!r}")
    trials = int(support["trials"])
    expected_rows = int(support["case_count"])
    expected_group_count = expected_rows // trials
    observed_rows = int(summary.get("row_count", 0))
    observed_group_count = int(summary.get("group_count", 0))
    problems = list(summary.get("problems") or [])
    if not allow_incomplete:
        if observed_rows != expected_rows:
            problems.append(f"row_count={observed_rows}, expected={expected_rows}")
        if observed_group_count != expected_group_count:
            problems.append(f"group_count={observed_group_count}, expected={expected_group_count}")

    grouped: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
    for row in summary.get("groups", []):
        if row.get("lane") != lane:
            problems.append(f"unexpected lane in summary group: {row.get('lane')!r}")
            continue
        grouped[identity(row)].append(row)

    families: list[dict[str, Any]] = []
    for rows in grouped.values():
        rows.sort(key=lambda row: int(row["clients"]))
        first = rows[0]
        expected = expected_clients(support, str(first["scenario"]))
        actual = [int(row["clients"]) for row in rows]
        missing = sorted(set(expected) - set(actual))
        extra = sorted(set(actual) - set(expected))
        duplicate = len(actual) != len(set(actual))
        complete = not missing and not extra and not duplicate
        if not complete and not allow_incomplete:
            problems.append(
                f"{first['engine']}/{first['scenario']}/{first['workload']}/{first['durability']}: "
                f"clients={actual}, expected={expected}"
            )
        by_client = {int(row["clients"]): row for row in rows}
        base = by_client.get(1)
        max_client = max(expected)
        max_row = by_client.get(max_client)
        rates = {str(client): float(row["ops_per_s_median"]) for client, row in sorted(by_client.items())}
        family = {
            "scenario": first["scenario"],
            "engine": first["engine"],
            "engine_version": first["engine_version"],
            "durability": first["durability"],
            "workload": first["workload"],
            "records": first["records"],
            "ops_requested": first["ops_requested"],
            "value_bytes": first["value_bytes"],
            "txn_size": first["txn_size"],
            "scan_len": first["scan_len"],
            "expected_clients": expected,
            "observed_clients": actual,
            "missing_clients": missing,
            "complete": complete,
            "ops_per_s_by_client": rates,
            "c1_ops_per_s": float(base["ops_per_s_median"]) if base else None,
            "max_clients": max_client,
            "max_client_ops_per_s": float(max_row["ops_per_s_median"]) if max_row else None,
            "speedup_at_max_clients": max_row.get("speedup_vs_c1") if max_row else None,
            "parallel_efficiency_at_max_clients": max_row.get("parallel_efficiency_vs_c1") if max_row else None,
            "p99_read_or_op_multiplier_at_max_clients": max_row.get("p99_read_multiplier_vs_c1") if max_row else None,
            "p99_write_txn_multiplier_at_max_clients": max_row.get("p99_write_txn_multiplier_vs_c1") if max_row else None,
            "client_max_min_ratio_at_max_clients": max_row.get("client_throughput_max_min_ratio_median") if max_row else None,
            "cpu_cores_at_max_clients": max_row.get("cpu_cores_median") if max_row else None,
            "conflict_retries_per_k_write_ops_at_max_clients": max_row.get("write_conflict_retries_per_k_write_ops_median") if max_row else None,
        }
        families.append(family)

    families.sort(key=lambda f: (str(f["scenario"]), str(f["workload"]), str(f["durability"]), str(f["engine"])))
    complete_count = sum(bool(f["complete"]) for f in families)
    return {
        "lane": lane,
        "profile": support.get("profile"),
        "trials": trials,
        "expected_rows": expected_rows,
        "observed_rows": observed_rows,
        "expected_group_count": expected_group_count,
        "observed_group_count": observed_group_count,
        "family_count": len(families),
        "complete_family_count": complete_count,
        "incomplete_family_count": len(families) - complete_count,
        "rejected_pressure_attempts": rejected_pressure_attempts(run_dir) if run_dir else None,
        "problems": problems,
        "families": families,
    }


def fmt(value: Any, digits: int = 2) -> str:
    if value is None:
        return "—"
    return f"{float(value):.{digits}f}"


def render_markdown(report: dict[str, Any]) -> str:
    lines = [
        f"# {report['lane']} scaling report",
        "",
        (
            f"Rows: **{report['observed_rows']}/{report['expected_rows']}**; summary groups: "
            f"**{report['observed_group_count']}/{report['expected_group_count']}**; families observed: "
            f"**{report['family_count']}**; complete client-set families: **{report['complete_family_count']}**."
        ),
    ]
    if report.get("rejected_pressure_attempts") is not None:
        lines.append(f"Archived transient-pressure attempts: **{report['rejected_pressure_attempts']}**.")
    if report["problems"]:
        lines += ["", "## Problems", "", *[f"- {problem}" for problem in report["problems"]]]
    lines += [
        "",
        "## Scaling families",
        "",
        "| scenario | workload | durability | engine | clients | c1 ops/s | max-c ops/s | speedup | efficiency | p99 read/op x | p99 write-txn x | client max/min | CPU cores | conflict retries/k write ops |",
        "| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for f in report["families"]:
        clients = ",".join(map(str, f["observed_clients"]))
        lines.append(
            f"| {f['scenario']} | {f['workload']} | {f['durability']} | {f['engine']} | {clients} | "
            f"{fmt(f['c1_ops_per_s'], 1)} | {fmt(f['max_client_ops_per_s'], 1)} | "
            f"{fmt(f['speedup_at_max_clients'])} | {fmt(f['parallel_efficiency_at_max_clients'])} | "
            f"{fmt(f['p99_read_or_op_multiplier_at_max_clients'])} | {fmt(f['p99_write_txn_multiplier_at_max_clients'])} | "
            f"{fmt(f['client_max_min_ratio_at_max_clients'])} | {fmt(f['cpu_cores_at_max_clients'])} | "
            f"{fmt(f['conflict_retries_per_k_write_ops_at_max_clients'])} |"
        )
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description="Build family-level scaling report from a concurrency run summary")
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("--json-out", type=Path)
    parser.add_argument("--markdown-out", type=Path)
    parser.add_argument("--allow-incomplete", action="store_true")
    args = parser.parse_args()
    summary_path = args.run_dir / "summary.json"
    support_path = args.run_dir / "support.json"
    if not summary_path.is_file() or not support_path.is_file():
        parser.error("run_dir must contain summary.json and support.json")
    report = build_report(
        json.loads(summary_path.read_text()),
        json.loads(support_path.read_text()),
        run_dir=args.run_dir,
        allow_incomplete=args.allow_incomplete,
    )
    text = render_markdown(report)
    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    if args.markdown_out:
        args.markdown_out.parent.mkdir(parents=True, exist_ok=True)
        args.markdown_out.write_text(text)
    else:
        print(text, end="")
    return 1 if report["problems"] and not args.allow_incomplete else 0


if __name__ == "__main__":
    raise SystemExit(main())
