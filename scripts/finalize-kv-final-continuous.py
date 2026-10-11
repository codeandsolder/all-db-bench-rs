#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations
import argparse, hashlib, json, os, re, statistics, subprocess, sys
from pathlib import Path
from typing import Any
STRATEGY="final-all-groups-continuous-v6"; ADMISSION="pre-io+pre/continuous/post-external-v3"; HEX64=re.compile(r"^[0-9a-f]{64}$")

def sha256(p:Path)->str:
    h=hashlib.sha256()
    with p.open("rb") as f:
        for c in iter(lambda:f.read(1024*1024),b""): h.update(c)
    return h.hexdigest()

def atomic_write(p:Path,text:str)->None:
    p.parent.mkdir(parents=True,exist_ok=True); t=p.with_name(f".{p.name}.tmp.{os.getpid()}")
    try:
        with t.open("w") as f: f.write(text); f.flush(); os.fsync(f.fileno())
        os.replace(t,p)
    finally: t.unlink(missing_ok=True)

def slug(v:str)->str: return "".join(c if c.isalnum() or c in "-_" else "-" for c in v)
def run_id(g:dict[str,Any],prefix:str)->str: return f"{prefix}-{slug(str(g['engine']))}-{slug(str(g['durability']))}-{slug(str(g['workload']))}-e{int(g['suggested_effective_ops'])}-t{int(g['suggested_trials'])}"
def ident(x:dict[str,Any])->tuple[str,str,str,int]: return str(x["engine"]),str(x["durability"]),str(x["workload"]),int(x["records"])
def rows(p:Path)->list[dict[str,Any]]: return [json.loads(l) for l in p.read_text().splitlines() if l.strip()]

def classify(rs:list[dict[str,Any]],t:dict[str,Any])->dict[str,Any]:
    e=[float(r["elapsed_s"]) for r in rs]; q=[float(r["ops_per_s"]) for r in rs]; med=statistics.median(e); total=sum(e); mr=statistics.median(q); mean=statistics.fmean(q); cv=statistics.stdev(q)/mean if len(q)>1 and mean else 0.0; spread=(max(q)-min(q))/mr if mr else float("inf")
    ro=str(rs[0]["workload"]) in set(t["read_only_workloads"]); under=med<float(t["read_only_min_seconds"]) if ro else total<float(t["stateful_min_total_seconds"]); var=cv>float(t["max_cv"]) or spread>float(t["max_relative_spread"])
    return {"final_status":"undersized" if under else ("variable" if var else "accepted"),"median_elapsed_s":med,"total_elapsed_s":total,"throughput_cv":cv,"throughput_relative_spread":spread}

def support_provenance(s:dict[str,Any],rid:str,binsha:str)->dict[str,Any]:
    if s.get("lane")!="kv" or s.get("profile")!="quick": raise ValueError(f"wrong lane/profile: {rid}")
    if s.get("admission_policy")!=ADMISSION: raise ValueError(f"wrong admission: {rid}")
    if s.get("benchmark_binary_sha256")!=binsha: raise ValueError(f"wrong binary: {rid}")
    if s.get("imported_from_run") or s.get("imported_cases_manifest_sha256"): raise ValueError(f"imported cases are forbidden in v6: {rid}")
    out={k:s.get(k) for k in (
        "runner_sha256","kv_matrix_policy_sha256","performance_common_sha256","continuous_noise_common_sha256","noise_guard_sha256","continuous_noise_guard_sha256",
        "continuous_noise_sample_ms","continuous_noise_max_cpu_percent","continuous_noise_max_io_average_mib_s","continuous_noise_max_io_rate_mib_s",
        "build_profile","hostname","machine_id_sha256","filesystem","source","initial_min_free_gib","case_min_free_gib",
    )}
    if any(v in (None,"") for v in out.values()): raise ValueError(f"missing run provenance: {rid}")
    for key in ("runner_sha256","kv_matrix_policy_sha256","performance_common_sha256","continuous_noise_common_sha256","noise_guard_sha256","continuous_noise_guard_sha256","machine_id_sha256"):
        if not HEX64.fullmatch(str(out[key])): raise ValueError(f"malformed {key}: {rid}")
    if out["build_profile"]!="external": raise ValueError(f"unexpected build profile: {rid}")
    for key in ("continuous_noise_sample_ms","continuous_noise_max_cpu_percent","continuous_noise_max_io_average_mib_s","continuous_noise_max_io_rate_mib_s","initial_min_free_gib","case_min_free_gib"):
        if float(out[key]) <= 0: raise ValueError(f"non-positive {key}: {rid}")
    return out

def finalize(source:Path,auditp:Path,planp:Path,repo:Path,prefix:str,out:Path,binsha:str)->dict[str,Any]:
    sr,sm,ss=source/"results.ndjson",source/"manifest.json",source/"summary.json"
    for p in (sr,sm,ss,auditp,planp):
        if not p.is_file(): raise ValueError(f"missing input: {p}")
    plan=json.loads(planp.read_text()); audit=json.loads(auditp.read_text())
    if plan.get("quality_policy_version")!=1 or plan.get("kv_final_continuous_plan_version")!=1 or plan.get("strategy")!=STRATEGY or plan.get("publication_admission_policy")!=ADMISSION or int(plan.get("group_count",-1))!=180: raise ValueError("bad v6 plan identity")
    if plan.get("source_results_sha256")!=sha256(sr) or plan.get("source_manifest_sha256")!=sha256(sm) or plan.get("source_summary_sha256")!=sha256(ss): raise ValueError("v6 source hash mismatch")
    srcids={ident(r) for r in rows(sr)}; groups=list(plan.get("groups",[])); gids={ident(g) for g in groups}
    if len(srcids)!=180 or len(gids)!=180 or srcids!=gids: raise ValueError("v6 plan does not cover all 180 source identities")
    thresholds=audit.get("thresholds"); combined=[]; sources=[]; supports={}; cids=set(); counts={"accepted":0,"variable":0,"undersized":0}
    for g in groups:
        rid=run_id(g,prefix); rd=repo/"results"/"runs"/rid; rp,sp,up=rd/"results.ndjson",rd/"summary.json",rd/"support.json"
        for p in (rp,sp,up):
            if not p.is_file(): raise ValueError(f"incomplete v6 run {rid}: {p.name}")
        n=int(g["suggested_trials"]); op=int(g["suggested_effective_ops"]); rec=int(g["records"]); summary=json.loads(sp.read_text())
        if int(summary.get("row_count",-1))!=n or int(summary.get("group_count",-1))!=1 or summary.get("problems"): raise ValueError(f"bad summary: {rid}")
        sup=json.loads(up.read_text()); provenance=support_provenance(sup,rid,binsha)
        runner_ops=int(g["runner_ops_override"])
        if (int(sup.get("trials",-1))!=n or int(sup.get("records",-1))!=rec or int(sup.get("ops",-1))!=runner_ops
                or sup.get("engines")!=g["engine"] or sup.get("durabilities")!=g["durability"] or sup.get("workloads")!=g["workload"]):
            raise ValueError(f"support geometry mismatch: {rid}")
        cids.add(json.dumps(provenance,sort_keys=True,separators=(",",":"))); supports[rid]=sha256(up); rr=rows(rp)
        if len(rr)!=n or sorted(int(r["trial"]) for r in rr)!=list(range(1,n+1)): raise ValueError(f"trial mismatch: {rid}")
        if {ident(r) for r in rr}!={ident(g)} or {int(r["ops_requested"]) for r in rr}!={op}: raise ValueError(f"identity/work mismatch: {rid}")
        q=classify(rr,thresholds); counts[q["final_status"]]+=1
        if q["final_status"]=="undersized": raise ValueError(f"fresh v6 group remains undersized: {rid}")
        combined.extend(rr); sources.append({"engine":g["engine"],"durability":g["durability"],"workload":g["workload"],"records":rec,"source":"continuous-confirmation-v6","run_id":rid,"ops_requested":op,"runner_ops_override":runner_ops,"trials":n,"support_sha256":supports[rid],**q})
    if len(cids)!=1: raise ValueError("inconsistent run provenance")
    run_identity=json.loads(next(iter(cids)))
    continuous_identity={k:run_identity[k] for k in ("noise_guard_sha256","continuous_noise_guard_sha256","continuous_noise_sample_ms","continuous_noise_max_cpu_percent","continuous_noise_max_io_average_mib_s","continuous_noise_max_io_rate_mib_s")}
    expected=int(plan.get("expected_rows",-1))
    if len(combined)!=expected: raise ValueError(f"combined rows {len(combined)} != {expected}")
    out.mkdir(parents=True,exist_ok=True); complete=out/"complete.json"; complete.unlink(missing_ok=True); ro,mo,so,smd=out/"results.ndjson",out/"manifest.json",out/"summary.json",out/"summary.md"
    manifest={"selection_version":6,"complete":True,"full_continuous_confirmation":True,"admission_policy":ADMISSION,"selected_rows":len(combined),"selected_groups":180,"selected_stock_groups":0,"selected_resize_groups":0,"selected_quality_repair_groups":180,"final_status_counts":counts,"source_selection":str(source),"source_results_sha256":sha256(sr),"source_manifest_sha256":sha256(sm),"source_summary_sha256":sha256(ss),"plan_sha256":sha256(planp),"benchmark_binary_sha256":binsha,"source_support_sha256":dict(sorted(supports.items())),"run_provenance_identity":run_identity,"continuous_admission_identity":continuous_identity,"sources":sources}
    atomic_write(ro,"".join(json.dumps(r,sort_keys=True,separators=(",",":"))+"\n" for r in combined)); atomic_write(mo,json.dumps(manifest,indent=2,sort_keys=True)+"\n")
    subprocess.run([sys.executable,str(repo/"scripts"/"summarize.py"),str(ro),"--json-out",str(so),"--markdown-out",str(smd)],cwd=repo,check=True)
    summary=json.loads(so.read_text())
    if int(summary.get("row_count",-1))!=len(combined) or int(summary.get("group_count",-1))!=180 or summary.get("problems"): raise ValueError("final summary validation failed")
    c={"completion_version":1,"selection_version":6,"full_continuous_confirmation":True,"admission_policy":ADMISSION,"selected_rows":len(combined),"selected_groups":180,"results_sha256":sha256(ro),"manifest_sha256":sha256(mo),"summary_sha256":sha256(so),"plan_sha256":sha256(planp)}; atomic_write(complete,json.dumps(c,indent=2,sort_keys=True)+"\n"); return c

def main()->int:
    p=argparse.ArgumentParser(); p.add_argument("--source-selected",type=Path,required=True); p.add_argument("--audit",type=Path,required=True); p.add_argument("--plan",type=Path,required=True); p.add_argument("--repo",type=Path,required=True); p.add_argument("--run-prefix",required=True); p.add_argument("--out-dir",type=Path,required=True); p.add_argument("--expected-bench-sha256",required=True); a=p.parse_args()
    try: c=finalize(a.source_selected,a.audit,a.plan,a.repo,a.run_prefix,a.out_dir,a.expected_bench_sha256)
    except (OSError,ValueError,json.JSONDecodeError,subprocess.CalledProcessError) as e: raise SystemExit(str(e)) from e
    print(json.dumps(c,sort_keys=True)); return 0
if __name__=="__main__": raise SystemExit(main())
