#!/usr/bin/env python3
from __future__ import annotations
import argparse, hashlib, json, os, subprocess
from pathlib import Path

def sha256(path: Path) -> str:
    h=hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda:f.read(1024*1024),b""):
            h.update(chunk)
    return h.hexdigest()

def checked_runner_commit(repo: Path) -> str:
    tracked_dirty=subprocess.check_output(
        ["git", "status", "--porcelain", "--untracked-files=no"], cwd=repo, text=True
    ).strip()
    if tracked_dirty:
        raise SystemExit("runner checkout has tracked modifications; refusing readiness certificate")
    return subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()

def main() -> int:
    ap=argparse.ArgumentParser()
    ap.add_argument("--selected", type=Path, required=True)
    ap.add_argument("--binary-manifest", type=Path, required=True)
    ap.add_argument("--expected-commit", required=True)
    ap.add_argument("--repo", type=Path, required=True)
    ap.add_argument("--stock-run-id", required=True)
    ap.add_argument("--expected-admission-policy", required=True)
    ap.add_argument("--out", type=Path, required=True)
    a=ap.parse_args()
    runner_repo_commit=checked_runner_commit(a.repo)
    manifest=json.loads((a.selected/"manifest.json").read_text())
    summary=json.loads((a.selected/"summary.json").read_text())
    bin_manifest=json.loads(a.binary_manifest.read_text())
    rows=[json.loads(line) for line in (a.selected/"results.ndjson").read_text().splitlines() if line.strip()]
    if not manifest.get("complete") or int(manifest.get("selected_groups",-1)) != 40:
        raise SystemExit("record selection is not complete at 40 groups")
    if int(summary.get("group_count",-1)) != 40 or summary.get("problems"):
        raise SystemExit("record summary is not a clean 40-group corpus")
    if int(summary.get("row_count",-1)) != len(rows) or int(manifest.get("selected_rows",-1)) != len(rows):
        raise SystemExit("record row-count provenance mismatch")
    reads={r.get("read_materialization") for r in rows}
    writes={r.get("write_materialization") for r in rows}
    if reads != {"full-record-v1"}:
        raise SystemExit(f"unexpected read semantics: {sorted(map(str,reads))}")
    if writes != {"no-return-v1"}:
        raise SystemExit(f"unexpected write semantics: {sorted(map(str,writes))}")
    source_supports = {}
    selected_source_run_ids = set()
    if int(manifest.get("selected_stock_groups", 0)) > 0:
        selected_source_run_ids.add(a.stock_run_id)
    selected_source_run_ids.update(
        str(item["run_id"])
        for item in manifest.get("sources", [])
        if item.get("source") in {"resize", "quality-repair"} and item.get("run_id")
    )
    expected_selected_source_runs = (
        (1 if int(manifest.get("selected_stock_groups", 0)) > 0 else 0)
        + int(manifest.get("selected_resize_groups", 0))
        + int(manifest.get("selected_quality_repair_groups", 0))
    )
    if len(selected_source_run_ids) != expected_selected_source_runs:
        raise SystemExit(
            "selected source-run provenance mismatch: "
            f"manifest={expected_selected_source_runs} runs={len(selected_source_run_ids)}"
        )
    source_run_ids = [a.stock_run_id]
    source_run_ids.extend(
        sorted(
            {
                str(item["run_id"])
                for item in manifest.get("sources", [])
                if item.get("source") in {"resize", "quality-repair"} and item.get("run_id")
            }
        )
    )
    expected_followup_groups = (
        int(manifest.get("selected_resize_groups", 0))
        + int(manifest.get("selected_quality_repair_groups", 0))
    )
    if len(source_run_ids) - 1 != expected_followup_groups:
        raise SystemExit(
            f"followup source provenance mismatch: manifest={expected_followup_groups} support_runs={len(source_run_ids)-1}"
        )
    for run_id in source_run_ids:
        support_path = a.repo / "results" / "runs" / run_id / "support.json"
        if not support_path.is_file():
            raise SystemExit(f"missing source support: {support_path}")
        support = json.loads(support_path.read_text())
        if support.get("admission_policy") != a.expected_admission_policy:
            raise SystemExit(
                f"unexpected admission policy for {run_id}: {support.get('admission_policy')!r}"
            )
        source_supports[run_id] = sha256(support_path)
    selected_noise_guards = {
        json.loads((a.repo / "results" / "runs" / run_id / "support.json").read_text()).get("noise_guard_sha256")
        for run_id in selected_source_run_ids
    }
    if None in selected_noise_guards or "" in selected_noise_guards or len(selected_noise_guards) != 1:
        raise SystemExit(
            "selected performance sources have inconsistent noise guards: "
            f"{sorted(map(str, selected_noise_guards))}"
        )
    selected_noise_guard_sha256 = next(iter(selected_noise_guards))
    if bin_manifest.get("repo_commit") != a.expected_commit:
        raise SystemExit("binary manifest commit mismatch")
    if bin_manifest.get("read_materialization") != "full-record-v1" or bin_manifest.get("write_materialization") != "no-return-v1":
        raise SystemExit("binary manifest semantic mismatch")
    binaries=bin_manifest.get("binaries")
    if not isinstance(binaries,dict) or len(binaries) != 6:
        raise SystemExit("binary manifest does not describe exactly six record binaries")
    for name,item in binaries.items():
        path=Path(item.get("path", ""))
        expected=item.get("sha256")
        if not path.is_file() or not expected or sha256(path) != expected:
            raise SystemExit(f"pinned binary verification failed: {name}")
    out={
        "ready_version":4,
        "repo_commit":a.expected_commit,
        "binary_repo_commit":a.expected_commit,
        "runner_repo_commit":runner_repo_commit,
        "admission_policy":a.expected_admission_policy,
        "source_support_sha256":source_supports,
        "selected_noise_guard_sha256":selected_noise_guard_sha256,
        "group_count":40,
        "row_count":len(rows),
        "read_materialization":"full-record-v1",
        "write_materialization":"no-return-v1",
        "results_sha256":sha256(a.selected/"results.ndjson"),
        "selection_manifest_sha256":sha256(a.selected/"manifest.json"),
        "summary_sha256":sha256(a.selected/"summary.json"),
        "binary_manifest_sha256":sha256(a.binary_manifest),
    }
    a.out.parent.mkdir(parents=True,exist_ok=True)
    tmp=a.out.with_suffix(a.out.suffix+".tmp")
    tmp.write_text(json.dumps(out,indent=2,sort_keys=True)+"\n")
    os.replace(tmp,a.out)
    print(json.dumps(out,sort_keys=True))
    return 0
if __name__=="__main__": raise SystemExit(main())
