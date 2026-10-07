#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations
import argparse, copy, hashlib, json, math
from collections import defaultdict
from pathlib import Path
from typing import Any

def sha256(path: Path) -> str:
    h=hashlib.sha256()
    with path.open("rb") as f:
        for c in iter(lambda:f.read(1024*1024), b""): h.update(c)
    return h.hexdigest()

def case_set_sha256(paths: list[Path]) -> str:
    h=hashlib.sha256()
    for p in sorted(paths, key=lambda p:p.name):
        h.update(p.name.encode()); h.update(b"\0"); h.update(hashlib.sha256(p.read_bytes()).digest())
    return h.hexdigest()

def client_key(item: dict[str,Any]) -> tuple[Any,...]:
    return (item["scenario"],item["engine"],item["durability"],item["workload"],int(item["clients"]))
def family_key(item: dict[str,Any]) -> tuple[Any,...]:
    return (item["scenario"], item["engine"], item["durability"], item["workload"])
def rounded_ops(value: float) -> int:
    if not math.isfinite(value) or value <= 0: raise ValueError(f"invalid operation recommendation: {value}")
    e=math.floor(math.log10(value)); s=10**e; n=value/s
    for step in (1.0,2.0,5.0,10.0):
        if n <= step: return max(1,int(step*s))
    raise AssertionError("unreachable")

def _validate_recovery_run(run_dir: Path):
    sp=run_dir/"support.json"; smp=run_dir/"summary.json"
    if not sp.is_file() or not smp.is_file(): raise ValueError(f"recovery run is incomplete: {run_dir}")
    support=json.loads(sp.read_text()); summary=json.loads(smp.read_text())
    expected=int(support.get("case_count",-1)); paths=sorted((run_dir/"cases").glob("*.json")); failures=run_dir/"failures.ndjson"
    if expected <= 0 or len(paths) != expected: raise ValueError(f"recovery case count mismatch: {len(paths)} != {expected}")
    if int(summary.get("row_count",-1)) != expected or int(summary.get("group_count",-1)) != expected: raise ValueError("recovery summary count mismatch")
    if summary.get("problems"): raise ValueError(f"recovery summary has problems: {summary['problems']}")
    if failures.is_file() and failures.stat().st_size: raise ValueError("recovery run has failures")
    return [json.loads(p.read_text()) for p in paths], {"recovery_run_dir":str(run_dir),"recovery_support_sha256":sha256(sp),"recovery_summary_sha256":sha256(smp),"recovery_case_set_sha256":case_set_sha256(paths)}

def _recompute_families(report: dict[str,Any]) -> None:
    t=report["thresholds"]; floor=float(t["floor_seconds"]); target=float(t["target_seconds"]); severe=float(t["severe_seconds"]); max_trials=int(t["stateful_max_trials"])
    safe=set(t["safe_more_ops_workloads"]); state_changing=set(t["state_changing_workloads"]); stock_trials=int(report["source_support"]["trials"])
    by_family=defaultdict(list)
    for g in report["client_groups"]:
        if int(g.get("observed_count",0))>0: by_family[family_key(g)].append(g)
    counts=defaultdict(int); families=[]
    for original in report["families"]:
        fk=family_key(original); observed=by_family.get(fk,[]); clients=[int(v) for v in original["clients"]]
        missing=sorted(set(clients)-{int(x["clients"]) for x in observed})
        e={"scenario":original["scenario"],"engine":original["engine"],"durability":original["durability"],"workload":original["workload"],"clients":clients,"missing_clients":missing,"current_records":int(original["current_records"]),"current_ops":int(original["current_ops"]),"observed_client_groups":len(observed),"planned_client_groups":int(original["planned_client_groups"]),"semantics":original["semantics"]}
        workload=str(e["workload"])
        if not observed:
            e["status"]="needs-probe"; e["recommendation"]="collect-stock-probe"
        else:
            fastest=min(float(x["median_elapsed_s"]) for x in observed); max_rate=max(float(x["median_ops_per_s"]) for x in observed)
            e["fastest_observed_median_elapsed_s"]=fastest; e["max_observed_median_ops_per_s"]=max_rate
            if missing:
                e["status"]="needs-probe"; e["recommendation"]="collect-stock-probe"
            elif workload in safe:
                if fastest < floor:
                    e["status"]="undersized"; e["recommendation"]="more-ops"; e["suggested_ops"]=max(int(e["current_ops"]),rounded_ops(max_rate*target)); e["suggested_trials"]=stock_trials
                else:
                    e["status"]="sized"; e["recommendation"]="keep-stock"; e["suggested_ops"]=int(e["current_ops"]); e["suggested_trials"]=stock_trials
            else:
                needed=max(stock_trials,math.ceil(target/fastest)); e["suggested_ops"]=int(e["current_ops"]); e["suggested_trials"]=needed
                if fastest >= floor:
                    e["status"]="sized"; e["recommendation"]="keep-stock"
                elif needed <= max_trials and fastest >= severe:
                    e["status"]="undersized"; e["recommendation"]="more-fresh-trials"
                else:
                    e["status"]="redesign-required"; e["recommendation"]="state-preserving-longer-window"
            if workload in state_changing:
                provisional=max(stock_trials,math.ceil(target/fastest)); e["provisional_stateful_trials_needed"]=provisional
                if fastest < severe or provisional > max_trials: e["provisional_stateful_risk"]="redesign-required"
                elif fastest < floor: e["provisional_stateful_risk"]="more-fresh-trials"
                else: e["provisional_stateful_risk"]="sized"
        counts[e["status"]]+=1; families.append(e)
    report["families"]=families; report["counts"]=dict(sorted(counts.items())); report["observed_family_count"]=len(by_family); report["observed_stateful_redesign_risk_count"]=sum(x.get("provisional_stateful_risk")=="redesign-required" for x in families)

def recover(cached_audit_path: Path, recovery_run_dir: Path) -> dict[str,Any]:
    report=copy.deepcopy(json.loads(cached_audit_path.read_text()))
    if int(report.get("concurrency_sizing_policy_version",-1)) != 1: raise ValueError("unsupported cached sizing audit version")
    missing_cases=list(report.get("missing_probe_cases") or [])
    if not missing_cases: raise ValueError("cached sizing audit has no missing client groups")
    missing={client_key(x):x for x in missing_cases}
    if len(missing)!=len(missing_cases): raise ValueError("cached sizing audit has duplicate missing identities")
    rows,prov=_validate_recovery_run(recovery_run_dir)
    if len(rows)!=len(missing): raise ValueError(f"recovery row count {len(rows)} does not match missing count {len(missing)}")
    recovered={}
    for row in rows:
        key=client_key(row); planned=missing.get(key)
        if planned is None: raise ValueError(f"recovery result outside cached missing set: {key}")
        if key in recovered: raise ValueError(f"duplicate recovery identity: {key}")
        if int(row["records"])!=int(planned["records"]): raise ValueError(f"recovery records mismatch for {key}")
        if int(row["ops_requested"])!=int(planned["ops"]): raise ValueError(f"recovery ops mismatch for {key}")
        if int(row["ops_completed"])!=int(row["ops_requested"]): raise ValueError(f"partial recovery result for {key}")
        if int(row["trial"])!=int(planned["trial"]): raise ValueError(f"recovery trial mismatch for {key}")
        elapsed=float(row["elapsed_s"]); rate=float(row["ops_per_s"])
        if not math.isfinite(elapsed) or elapsed<=0 or not math.isfinite(rate) or rate<=0: raise ValueError(f"invalid recovery timing for {key}")
        recovered[key]=row
    groups={client_key(x):x for x in report["client_groups"]}
    if len(groups)!=len(report["client_groups"]): raise ValueError("cached client_groups contains duplicate identities")
    for key,row in recovered.items():
        g=groups.get(key)
        if g is None: raise ValueError(f"recovery group missing from cached audit: {key}")
        if int(g.get("observed_count",0))!=0: raise ValueError(f"recovery group was already observed in cached audit: {key}")
        g["observed_trials"]=[int(row["trial"])]; g["observed_count"]=1; g["median_elapsed_s"]=float(row["elapsed_s"]); g["median_ops_per_s"]=float(row["ops_per_s"]); g["min_elapsed_s"]=float(row["elapsed_s"]); g["max_elapsed_s"]=float(row["elapsed_s"])
    report["row_count"]=int(report["row_count"])+len(rows)
    report["observed_client_group_count"]=sum(int(x.get("observed_count",0))>0 for x in report["client_groups"])
    report["missing_probe_case_count"]=0; report["missing_probe_cases"]=[]
    report.setdefault("additional_run_dirs",[])
    run_str=str(recovery_run_dir)
    if run_str not in report["additional_run_dirs"]: report["additional_run_dirs"].append(run_str)
    report["recovery_provenance"]={"cached_audit_path":str(cached_audit_path),"cached_audit_sha256":sha256(cached_audit_path),"cached_row_count":int(report["row_count"])-len(rows),**prov}
    _recompute_families(report)
    if int(report["observed_client_group_count"]) != int(report["planned_client_group_count"]): raise ValueError(f"recovered audit still incomplete: {report['observed_client_group_count']} / {report['planned_client_group_count']} client groups")
    pending=[x for x in report["families"] if x.get("missing_clients") or x.get("status")=="needs-probe"]
    if pending: raise ValueError(f"recovered audit still has incomplete families: {pending[:3]}")
    return report

def main() -> int:
    ap=argparse.ArgumentParser(description="Complete a cached concurrency sizing audit from a small recovery run")
    ap.add_argument("cached_audit",type=Path); ap.add_argument("recovery_run",type=Path); ap.add_argument("--json-out",type=Path,required=True); args=ap.parse_args()
    try: report=recover(args.cached_audit,args.recovery_run)
    except (OSError,json.JSONDecodeError,KeyError,TypeError,ValueError) as e: raise SystemExit(str(e)) from e
    args.json_out.parent.mkdir(parents=True,exist_ok=True); args.json_out.write_text(json.dumps(report,indent=2,sort_keys=True)+"\n")
    print(json.dumps({"row_count":report["row_count"],"observed_client_group_count":report["observed_client_group_count"],"planned_client_group_count":report["planned_client_group_count"],"missing_probe_case_count":report["missing_probe_case_count"],"counts":report["counts"],"cached_audit_sha256":report["recovery_provenance"]["cached_audit_sha256"],"recovery_case_set_sha256":report["recovery_provenance"]["recovery_case_set_sha256"]},sort_keys=True))
    return 0
if __name__=="__main__": raise SystemExit(main())
