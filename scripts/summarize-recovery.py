# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import json
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any


def read_ndjson(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    rows: list[dict[str, Any]] = []
    for lineno, line in enumerate(path.read_text().splitlines(), 1):
        if not line.strip():
            continue
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError as exc:
            raise SystemExit(f"{path}:{lineno}: {exc}") from exc
    return rows


def load_cases(run_dir: Path) -> dict[str, dict[str, Any]]:
    cases: dict[str, dict[str, Any]] = {}
    for path in sorted((run_dir / "cases").glob("*.json")):
        case_id = path.stem
        if case_id in cases:
            raise SystemExit(f"duplicate case id: {case_id}")
        cases[case_id] = json.loads(path.read_text())
    return cases


def main() -> None:
    ap = argparse.ArgumentParser(description="Summarize crash/power-loss recovery campaigns")
    ap.add_argument("run_dir", type=Path)
    ap.add_argument("--json-out", type=Path)
    ap.add_argument("--markdown-out", type=Path)
    args = ap.parse_args()

    run_dir = args.run_dir.resolve()
    cases = load_cases(run_dir)
    failures = read_ndjson(run_dir / "failures.ndjson")
    relaxed_losses = read_ndjson(run_dir / "relaxed-ack-losses.ndjson")
    sidecars = {
        p.stem: json.loads(p.read_text())
        for p in sorted((run_dir / "powerloss").glob("*.json"))
    }

    failure_by_case = {str(row.get("case_id")): row for row in failures}
    failure_stages = Counter(str(row.get("stage", "unknown")) for row in failures)
    grouped: dict[tuple[str, str], list[tuple[str, dict[str, Any]]]] = defaultdict(list)
    for case_id, row in cases.items():
        grouped[(str(row.get("engine", "unknown")), str(row.get("durability", "unknown")))].append((case_id, row))

    groups: list[dict[str, Any]] = []
    for (engine, durability), rows in sorted(grouped.items()):
        verifications = [row.get("verification") or {} for _, row in rows]
        structural_bad = sum(
            int(
                int(v.get("prefix_present_after_gap", 0)) != 0
                or int(v.get("tail_present_after_gap", 0)) != 0
                or not bool(v.get("transaction_atomic_tail", True))
            )
            for v in verifications
        )
        groups.append(
            {
                "engine": engine,
                "durability": durability,
                "completed_cases": len(rows),
                "verification_ok": sum(bool(v.get("verification_ok", False)) for v in verifications),
                "verification_failed": sum(not bool(v.get("verification_ok", False)) for v in verifications),
                "structural_failures": structural_bad,
                "max_missing_prefix_records": max((int(v.get("missing_prefix_records", 0)) for v in verifications), default=0),
                "max_prefix_present_after_gap": max((int(v.get("prefix_present_after_gap", 0)) for v in verifications), default=0),
                "max_tail_present_after_gap": max((int(v.get("tail_present_after_gap", 0)) for v in verifications), default=0),
                "max_unreported_tail_records": max((int(v.get("tail_prefix_present", 0)) for v in verifications), default=0),
                "harness_failures": sum(case_id in failure_by_case for case_id, _ in rows),
            }
        )

    summary: dict[str, Any] = {
        "run_dir": str(run_dir),
        "support": json.loads((run_dir / "support.json").read_text()) if (run_dir / "support.json").exists() else None,
        "completed_cases": len(cases),
        "failure_records": len(failures),
        "failure_stages": dict(sorted(failure_stages.items())),
        "relaxed_ack_loss_cases": len(relaxed_losses),
        "powerloss_sidecars": len(sidecars),
        "all_completed_verifications_ok": all(
            bool((row.get("verification") or {}).get("verification_ok", False)) for row in cases.values()
        ) if cases else False,
        "all_sync_verifications_ok": all(
            str(row.get("durability")) != "sync"
            or bool((row.get("verification") or {}).get("verification_ok", False))
            for row in cases.values()
        ) if cases else False,
        "all_structurally_valid": all(
            int((row.get("verification") or {}).get("prefix_present_after_gap", 0)) == 0
            and int((row.get("verification") or {}).get("tail_present_after_gap", 0)) == 0
            and bool((row.get("verification") or {}).get("transaction_atomic_tail", True))
            for row in cases.values()
        ) if cases else False,
        "groups": groups,
    }

    text = json.dumps(summary, indent=2, sort_keys=True)
    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(text + "\n")

    lines = [
        "# Recovery campaign summary",
        "",
        f"- Completed case results: {len(cases)}",
        f"- Harness/failure records: {len(failures)}",
        f"- Relaxed acknowledged-loss observations: {len(relaxed_losses)}",
        f"- Power-loss sidecars: {len(sidecars)}",
        "",
        "| Engine | Durability | Cases | OK | Failed | Structural | Max missing prefix | Max tail | Harness failures |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for g in groups:
        lines.append(
            f"| {g['engine']} | {g['durability']} | {g['completed_cases']} | "
            f"{g['verification_ok']} | {g['verification_failed']} | {g['structural_failures']} | "
            f"{g['max_missing_prefix_records']} | {g['max_unreported_tail_records']} | {g['harness_failures']} |"
        )
    if failure_stages:
        lines += ["", "## Failure stages"]
        for stage, count in sorted(failure_stages.items()):
            lines.append(f"- `{stage}`: {count}")
    markdown = "\n".join(lines) + "\n"
    if args.markdown_out:
        args.markdown_out.parent.mkdir(parents=True, exist_ok=True)
        args.markdown_out.write_text(markdown)
    if not args.json_out and not args.markdown_out:
        print(markdown, end="")


if __name__ == "__main__":
    main()
