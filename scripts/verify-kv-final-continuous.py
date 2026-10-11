#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations
import argparse, hashlib, json, os, subprocess
from pathlib import Path
ADMISSION="pre-io+pre/continuous/post-external-v3"

def sha256(p:Path)->str:
    h=hashlib.sha256()
    with p.open("rb") as f:
        for c in iter(lambda:f.read(1024*1024),b""): h.update(c)
    return h.hexdigest()

def runner_commit(repo:Path)->str:
    if subprocess.check_output(["git","status","--porcelain","--untracked-files=no"],cwd=repo,text=True).strip(): raise ValueError("runner checkout has tracked modifications")
    return subprocess.check_output(["git","rev-parse","HEAD"],cwd=repo,text=True).strip()

def verify(selected:Path,plan:Path,repo:Path,binsha:str)->dict:
    files={"results":selected/"results.ndjson","manifest":selected/"manifest.json","summary":selected/"summary.json","complete":selected/"complete.json"}
    for p in (*files.values(),plan):
        if not p.is_file(): raise ValueError(f"missing KV v6 artifact: {p}")
    m=json.loads(files["manifest"].read_text()); s=json.loads(files["summary"].read_text()); c=json.loads(files["complete"].read_text()); pl=json.loads(plan.read_text()); expected=int(pl.get("expected_rows",-1))
    if int(m.get("selection_version",-1))!=6 or not m.get("complete") or not m.get("full_continuous_confirmation") or m.get("admission_policy")!=ADMISSION: raise ValueError("KV v6 manifest identity mismatch")
    if int(c.get("selection_version",-1))!=6 or not c.get("full_continuous_confirmation") or c.get("admission_policy")!=ADMISSION: raise ValueError("KV v6 completion identity mismatch")
    if any(x!=180 for x in (int(m.get("selected_groups",-1)),int(s.get("group_count",-1)),int(c.get("selected_groups",-1)))): raise ValueError("KV v6 group count mismatch")
    if any(x!=expected for x in (int(m.get("selected_rows",-1)),int(s.get("row_count",-1)),int(c.get("selected_rows",-1)))) or s.get("problems"): raise ValueError("KV v6 row count/summary mismatch")
    if int(m.get("selected_quality_repair_groups",-1))!=180 or int(m.get("selected_stock_groups",-1))!=0 or int(m.get("selected_resize_groups",-1))!=0: raise ValueError("KV v6 retained historical measurements")
    if int(m.get("final_status_counts",{}).get("undersized",-1))!=0: raise ValueError("KV v6 contains undersized groups")
    if m.get("benchmark_binary_sha256")!=binsha: raise ValueError("KV v6 binary provenance mismatch")
    if c.get("results_sha256")!=sha256(files["results"]) or c.get("manifest_sha256")!=sha256(files["manifest"]) or c.get("summary_sha256")!=sha256(files["summary"]): raise ValueError("KV v6 payload hash mismatch")
    if c.get("plan_sha256")!=sha256(plan) or m.get("plan_sha256")!=sha256(plan): raise ValueError("KV v6 plan hash mismatch")
    ci=m.get("continuous_admission_identity"); req=("noise_guard_sha256","continuous_noise_guard_sha256","continuous_noise_sample_ms","continuous_noise_max_cpu_percent","continuous_noise_max_io_average_mib_s","continuous_noise_max_io_rate_mib_s")
    if not isinstance(ci,dict) or any(ci.get(k) in (None,"") for k in req): raise ValueError("KV v6 continuous provenance incomplete")
    sh=m.get("source_support_sha256")
    if not isinstance(sh,dict) or len(sh)!=180: raise ValueError("KV v6 support provenance incomplete")
    return {"ready_version":6,"runner_repo_commit":runner_commit(repo),"admission_policy":ADMISSION,"group_count":180,"row_count":expected,"benchmark_binary_sha256":binsha,"results_sha256":sha256(files["results"]),"selection_manifest_sha256":sha256(files["manifest"]),"summary_sha256":sha256(files["summary"]),"completion_sha256":sha256(files["complete"]),"plan_sha256":sha256(plan),"selected_noise_guard_sha256":ci["noise_guard_sha256"],"selected_continuous_noise_guard_sha256":ci["continuous_noise_guard_sha256"],"continuous_noise_sample_ms":ci["continuous_noise_sample_ms"],"continuous_noise_max_cpu_percent":ci["continuous_noise_max_cpu_percent"],"continuous_noise_max_io_average_mib_s":ci["continuous_noise_max_io_average_mib_s"],"continuous_noise_max_io_rate_mib_s":ci["continuous_noise_max_io_rate_mib_s"]}

def main()->int:
    p=argparse.ArgumentParser(); p.add_argument("--selected",type=Path,required=True); p.add_argument("--plan",type=Path,required=True); p.add_argument("--repo",type=Path,required=True); p.add_argument("--expected-bench-sha256",required=True); p.add_argument("--out",type=Path); a=p.parse_args()
    try: r=verify(a.selected,a.plan,a.repo,a.expected_bench_sha256)
    except (OSError,ValueError,json.JSONDecodeError,subprocess.CalledProcessError) as e: raise SystemExit(str(e)) from e
    if a.out:
        a.out.parent.mkdir(parents=True,exist_ok=True); t=a.out.with_suffix(a.out.suffix+".tmp"); t.write_text(json.dumps(r,indent=2,sort_keys=True)+"\n"); os.replace(t,a.out)
    print(json.dumps(r,sort_keys=True)); return 0
if __name__=="__main__": raise SystemExit(main())
