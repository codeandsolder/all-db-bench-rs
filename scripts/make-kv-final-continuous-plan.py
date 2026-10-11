#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations
import argparse, hashlib, json
from collections import defaultdict
from pathlib import Path
from typing import Any
STRATEGY = "final-all-groups-continuous-v6"
ADMISSION = "pre-io+pre/continuous/post-external-v3"

def sha256(path: Path) -> str:
    h=hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda:f.read(1024*1024), b""): h.update(chunk)
    return h.hexdigest()

def identity(item: dict[str,Any]) -> tuple[str,str,str,int]:
    return str(item["engine"]),str(item["durability"]),str(item["workload"]),int(item["records"])

def read_rows(path: Path) -> list[dict[str,Any]]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]

def runner_ops_for(workload:str,effective_ops:int)->int:
    return effective_ops*10 if workload=="tiny-txn" else effective_ops

def effective_ops_for(engine:str,workload:str,runner_ops:int,records:int)->int:
    if workload=="tiny-txn": return min(max(100,runner_ops//10),runner_ops)
    if engine!="lsmdb" or workload!="range-scan": return runner_ops
    return min(max(50,2*50_000_000//records),runner_ops)

def make_plan(selected:Path,*,min_trials:int)->dict[str,Any]:
    if min_trials<5: raise ValueError("publication confirmation requires at least five fresh trials")
    rp,mp,sp=selected/"results.ndjson",selected/"manifest.json",selected/"summary.json"
    for p in (rp,mp,sp):
        if not p.is_file(): raise ValueError(f"missing source selection artifact: {p}")
    rows=read_rows(rp); manifest=json.loads(mp.read_text()); summary=json.loads(sp.read_text())
    if (not manifest.get("complete") or int(manifest.get("selected_groups",-1))!=180 or int(summary.get("group_count",-1))!=180 or int(summary.get("row_count",-1))!=len(rows) or summary.get("problems")):
        raise ValueError("selected-final-v4 is not a clean complete 180-group corpus")
    if int(manifest.get("final_status_counts",{}).get("undersized",-1))!=0: raise ValueError("selected-final-v4 still contains undersized groups")
    grouped=defaultdict(list)
    for row in rows: grouped[identity(row)].append(row)
    if len(grouped)!=180: raise ValueError(f"source selection identity count mismatch: {len(grouped)}")
    sources={}
    for source in manifest.get("sources",[]):
        prefix=str(source["engine"]),str(source["durability"]),str(source["workload"])
        matches=[k for k in grouped if k[:3]==prefix]
        if len(matches)!=1: raise ValueError(f"cannot resolve source identity: {source}")
        key=matches[0]
        if key in sources: raise ValueError(f"duplicate source identity: {key}")
        sources[key]=source
    if set(sources)!=set(grouped): raise ValueError("source manifest and selected rows do not describe the same identities")
    groups=[]; estimated=0.0
    for key in sorted(grouped):
        engine,durability,workload,records=key; source=sources[key]; source_rows=grouped[key]
        ops_values={int(r["ops_requested"]) for r in source_rows}
        if len(ops_values)!=1: raise ValueError(f"mixed logical work in selected group: {key}")
        effective=next(iter(ops_values))
        if int(source.get("ops_requested",-1))!=effective: raise ValueError(f"manifest/result op mismatch for {key}")
        source_trials=int(source.get("trials",-1))
        if source_trials!=len(source_rows): raise ValueError(f"manifest/result trial mismatch for {key}")
        trials=max(min_trials,source_trials); runner=runner_ops_for(workload,effective)
        computed=effective_ops_for(engine,workload,runner,records)
        if computed!=effective: raise ValueError(f"cannot reproduce selected work for {key}: effective={effective} runner={runner} computed={computed}")
        median=float(source["median_elapsed_s"]); estimated+=median*trials
        groups.append({"engine":engine,"durability":durability,"workload":workload,"records":records,"status":"final-confirmation","resize_strategy":"quality-repair","suggested_effective_ops":effective,"runner_ops_override":runner,"suggested_trials":trials,"median_elapsed_s":median,"source_selected_trials":source_trials,"source_selected_status":str(source["final_status"]),"source_selected_run_id":source.get("run_id")})
    return {"quality_policy_version":1,"kv_final_continuous_plan_version":1,"strategy":STRATEGY,"publication_admission_policy":ADMISSION,"source_selection":str(selected),"source_results_sha256":sha256(rp),"source_manifest_sha256":sha256(mp),"source_summary_sha256":sha256(sp),"group_count":len(groups),"expected_rows":sum(int(g["suggested_trials"]) for g in groups),"minimum_fresh_trials":min_trials,"work_policy":"preserve each selected-final-v4 identity's audited records and logical op count; replace every measurement with fresh continuous-admission trials","trial_policy":"max(minimum_fresh_trials, selected-final-v4 source trial count)","estimated_measured_seconds_from_selected_medians":estimated,"groups":groups}

def main()->int:
    ap=argparse.ArgumentParser(); ap.add_argument("selected",type=Path); ap.add_argument("--min-trials",type=int,default=5); ap.add_argument("--out",type=Path,required=True); a=ap.parse_args()
    plan=make_plan(a.selected,min_trials=a.min_trials); a.out.parent.mkdir(parents=True,exist_ok=True); a.out.write_text(json.dumps(plan,indent=2,sort_keys=True)+"\n")
    print(json.dumps({"out":str(a.out),"group_count":plan["group_count"],"expected_rows":plan["expected_rows"],"estimated_measured_seconds":plan["estimated_measured_seconds_from_selected_medians"],"plan_sha256":sha256(a.out)},sort_keys=True)); return 0
if __name__=="__main__": raise SystemExit(main())
