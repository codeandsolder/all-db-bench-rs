#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations
import argparse, hashlib, json, math
from collections import defaultdict
from pathlib import Path
from typing import Any

TARGET_SECONDS=3.0
MAX_SLOW_SECONDS=15.0
FINAL_TRIALS=3

def sha256(path: Path)->str:
    h=hashlib.sha256()
    with path.open("rb") as f:
        for c in iter(lambda:f.read(1024*1024),b""): h.update(c)
    return h.hexdigest()

def case_set_sha256(paths:list[Path])->str:
    h=hashlib.sha256()
    for p in sorted(paths,key=lambda p:p.name):
        h.update(p.name.encode()); h.update(b"\0"); h.update(hashlib.sha256(p.read_bytes()).digest())
    return h.hexdigest()

def family_key(x:dict[str,Any])->tuple[Any,...]:
    return (x["scenario"],x["engine"],x["durability"],x["workload"],int(x["records"]),int(x["payload_bytes"]),int(x["txn_size"]),x["state_evolution"])
def client_key(x:dict[str,Any])->tuple[Any,...]: return family_key(x)+(int(x["clients"]),)
def result_key(x:dict[str,Any])->tuple[Any,...]: return client_key(x)+(int(x["trial"]),)

def load_results(plan_path:Path,run_dir:Path):
    plan=json.loads(plan_path.read_text())
    if int(plan.get("record_concurrency_plan_version",-1))!=1 or plan.get("kind")!="semantic-calibration": raise ValueError("not a record semantic calibration plan")
    support_path=run_dir/"support.json"; summary_path=run_dir/"summary.json"
    if not support_path.is_file() or not summary_path.is_file(): raise ValueError("calibration run incomplete")
    support=json.loads(support_path.read_text()); summary=json.loads(summary_path.read_text()); plan_sha=sha256(plan_path)
    if support.get("plan_sha256")!=plan_sha: raise ValueError("calibration support plan SHA mismatch")
    paths=sorted((run_dir/"cases").glob("*.json")); expected=int(plan["case_count"])
    if len(paths)!=expected or int(summary.get("row_count",-1))!=expected or int(summary.get("group_count",-1))!=expected or summary.get("problems"): raise ValueError("calibration case/summary count mismatch")
    failures=run_dir/"failures.ndjson"
    if failures.is_file() and failures.stat().st_size: raise ValueError("calibration run has failures")
    planned={result_key(x):x for x in plan["cases"]}
    if len(planned)!=expected: raise ValueError("duplicate calibration plan identity")
    results={}
    for p in paths:
        row=json.loads(p.read_text()); key=result_key(row)
        if key not in planned: raise ValueError(f"result outside calibration plan: {p.name}")
        if key in results: raise ValueError(f"duplicate calibration result: {key}")
        if int(row.get("ops_completed",-1))!=int(row.get("ops_requested",-2)): raise ValueError(f"partial calibration result: {p.name}")
        elapsed=float(row["elapsed_s"]); rate=float(row["ops_per_s"])
        if not math.isfinite(elapsed) or elapsed<=0 or not math.isfinite(rate) or rate<=0: raise ValueError(f"invalid calibration timing: {p.name}")
        results[key]=row
    if set(results)!=set(planned): raise ValueError("calibration results do not exactly cover plan")
    return plan,results,{"calibration_plan_sha256":plan_sha,"calibration_support_sha256":sha256(support_path),"calibration_summary_sha256":sha256(summary_path),"calibration_case_set_sha256":case_set_sha256(paths)}

def choose_ops(cases:list[dict[str,Any]],results:dict[tuple[Any,...],dict[str,Any]]):
    current={int(x["ops"]) for x in cases}
    if len(current)!=1: raise ValueError("family calibration ops mismatch")
    current_ops=current.pop(); rates=[float(results[result_key(x)]["ops_per_s"]) for x in cases]
    fastest=max(rates); slowest=min(rates); desired=math.ceil(fastest*TARGET_SECONDS); cap=math.floor(slowest*MAX_SLOW_SECONDS)
    workload=str(cases[0]["workload"]); txn=int(cases[0]["txn_size"]); max_clients=max(int(x["clients"]) for x in cases)
    if workload=="write-burst":
        if current_ops%txn: raise ValueError("calibration write-burst ops are not transaction aligned")
        desired=((desired+txn-1)//txn)*txn
        cap=(cap//txn)*txn
        minimum=max_clients*txn
    else:
        minimum=max_clients
    if cap >= minimum:
        selected=max(minimum,min(desired,cap))
    else:
        selected=minimum
    return selected,{"calibration_ops":current_ops,"fastest_ops_per_s":fastest,"slowest_ops_per_s":slowest,"desired_ops_for_target":desired,"max_ops_for_slow_window":cap,"minimum_ops":minimum,"projected_fastest_s":selected/fastest,"projected_slowest_s":selected/slowest,"window_limited":selected < desired,"reduced_from_calibration":selected < current_ops}

def build_final(plan_path:Path,run_dir:Path,trials:int=FINAL_TRIALS)->dict[str,Any]:
    if trials<1: raise ValueError("trials must be positive")
    plan,results,prov=load_results(plan_path,run_dir)
    families=defaultdict(list)
    for case in plan["cases"]: families[family_key(case)].append(case)
    final_cases=[]; selected=[]
    for fk,cases in sorted(families.items()):
        clients=sorted(int(x["clients"]) for x in cases)
        if len(clients)!=len(set(clients)): raise ValueError(f"duplicate client group in family: {fk}")
        ops,analysis=choose_ops(cases,results); template=cases[0]
        selected.append({"scenario":template["scenario"],"engine":template["engine"],"durability":template["durability"],"workload":template["workload"],"records":int(template["records"]),"payload_bytes":int(template["payload_bytes"]),"txn_size":int(template["txn_size"]),"state_evolution":template["state_evolution"],"clients":clients,"final_ops":ops,**analysis})
        for trial in range(1,trials+1):
            for base in cases:
                item=dict(base); item["ops"]=ops; item["trial"]=trial; final_cases.append(item)
    return {"record_concurrency_plan_version":1,"kind":"steady-scaling-final","expect_trials":trials,"target_seconds":TARGET_SECONDS,"max_slow_seconds":MAX_SLOW_SECONDS,"source_calibration_plan_sha256":sha256(plan_path),**prov,"family_count":len(selected),"families":selected,"case_count":len(final_cases),"cases":final_cases}

def main()->int:
    ap=argparse.ArgumentParser(description="Build final record concurrency scaling plan from one-shot calibration")
    ap.add_argument("calibration_plan",type=Path); ap.add_argument("calibration_run",type=Path); ap.add_argument("--out",type=Path,required=True); ap.add_argument("--trials",type=int,default=FINAL_TRIALS); a=ap.parse_args()
    try: plan=build_final(a.calibration_plan,a.calibration_run,a.trials)
    except (OSError,json.JSONDecodeError,KeyError,TypeError,ValueError) as e: raise SystemExit(str(e)) from e
    a.out.parent.mkdir(parents=True,exist_ok=True); a.out.write_text(json.dumps(plan,indent=2,sort_keys=True)+"\n")
    print(json.dumps({"family_count":plan["family_count"],"case_count":plan["case_count"],"window_limited_families":sum(x["window_limited"] for x in plan["families"])},sort_keys=True)); return 0
if __name__=="__main__": raise SystemExit(main())
