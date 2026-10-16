#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any

LEGACY_RUN_PREFIX = "20261006-kv-resize-v4"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def slug(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "-_" else "-" for ch in value)


def legacy_run_id(group: dict[str, Any]) -> str:
    return (
        f"{LEGACY_RUN_PREFIX}-{slug(str(group['engine']))}-"
        f"{slug(str(group['durability']))}-{slug(str(group['workload']))}-"
        f"e{int(group['suggested_effective_ops'])}-t{int(group['suggested_trials'])}"
    )


def build_plan(audit: dict[str, Any], manifest: dict[str, Any], *, manifest_sha256: str) -> dict[str, Any]:
    if audit.get("sizing_policy_version") != 2:
        raise ValueError(f"unsupported sizing policy: {audit.get('sizing_policy_version')!r}")
    if not manifest.get("complete"):
        raise ValueError("selected baseline manifest is incomplete")

    audit_by_run_id = {
        legacy_run_id(group): group
        for group in audit.get("groups", [])
        if group.get("status") == "undersized"
        and group.get("suggested_effective_ops") is not None
        and group.get("suggested_trials") is not None
    }

    suspect_sources = [
        source
        for source in manifest.get("sources", [])
        if int(source.get("rejected_pressure_attempts", 0)) > 0
    ]
    groups: list[dict[str, Any]] = []
    seen: set[str] = set()
    total_rejected = 0
    for source in suspect_sources:
        run_id = source.get("run_id")
        if not run_id:
            raise ValueError(
                "selected source with rejected pressure attempts has no run_id; "
                f"cannot reconstruct exact workload: {source}"
            )
        if run_id in seen:
            raise ValueError(f"duplicate suspect selected source: {run_id}")
        seen.add(run_id)
        group = audit_by_run_id.get(run_id)
        if group is None:
            raise ValueError(f"suspect selected source not found in sizing audit: {run_id}")
        if int(group["suggested_effective_ops"]) != int(source["ops_requested"]):
            raise ValueError(f"effective ops mismatch for {run_id}")
        if int(group["suggested_trials"]) != int(source["trials"]):
            raise ValueError(f"trial-count mismatch for {run_id}")
        if (
            group["engine"] != source["engine"]
            or group["durability"] != source["durability"]
            or group["workload"] != source["workload"]
        ):
            raise ValueError(f"identity mismatch for {run_id}")
        rejected = int(source["rejected_pressure_attempts"])
        total_rejected += rejected
        repair = dict(group)
        repair.update(
            {
                "status": source.get("final_status", group.get("status")),
                "resize_strategy": "quality-repair",
                "quality_repair_required": True,
                "admission_repair_reason": "legacy-post-selection",
                "legacy_run_id": run_id,
                "legacy_rejected_pressure_attempts": rejected,
            }
        )
        groups.append(repair)

    groups.sort(
        key=lambda group: (
            str(group["engine"]),
            str(group["durability"]),
            str(group["workload"]),
        )
    )
    if total_rejected != int(manifest.get("selected_followup_rejected_pressure_attempts", 0)):
        raise ValueError(
            "selected rejected-attempt total does not match manifest aggregate: "
            f"{total_rejected} != {manifest.get('selected_followup_rejected_pressure_attempts')}"
        )

    return {
        "quality_policy_version": 1,
        "admission_repair_policy_version": 1,
        "reason": (
            "Freshly remeasure every selected baseline follow-up that survived one or more "
            "legacy post-case pressure rejections. Preserve exact original op and trial geometry; "
            "change only admission to pre-io+pre/post-external-v2."
        ),
        "source_manifest_sha256": manifest_sha256,
        "legacy_run_prefix": LEGACY_RUN_PREFIX,
        "suspect_source_count": len(suspect_sources),
        "legacy_rejected_pressure_attempts": total_rejected,
        "groups": groups,
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Plan fresh admission-v2 repairs for selected KV baseline sources with legacy rejected attempts"
    )
    parser.add_argument("--audit", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    audit = json.loads(args.audit.read_text())
    manifest = json.loads(args.manifest.read_text())
    plan = build_plan(audit, manifest, manifest_sha256=sha256(args.manifest))
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(plan, indent=2, sort_keys=True) + "\n")
    print(
        f"planned={len(plan['groups'])} "
        f"legacy_rejected_attempts={plan['legacy_rejected_pressure_attempts']} "
        f"out={args.out}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
