#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$ROOT/scripts/concurrency-runner-common.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/cases" "$tmp/noise" "$tmp/stderr"
case_id=t1-test-rocksdb-relaxed-write-burst
python3 - "$tmp/cases/$case_id.json" <<'PY'
import json,sys
elapsed=0.03
json.dump({
    "trial":1,"engine":"rocksdb","durability":"relaxed","workload":"write-burst",
    "elapsed_s":elapsed,"ops_per_s":1_000_000.0,
    "measured_process":{"cpu_runtime_fraction_of_wall":1.0,"runqueue_wait_fraction_of_wall":0.04,"write_bytes":1234},
    "measured_system_delta":{"accounting_wall_ns":int(elapsed*1e9),"psi_cpu_some_us":0},
},open(sys.argv[1],"w"))
PY
set +e
concurrency_scrub_case_pressure "$ROOT" quick "$tmp" "$case_id" "$tmp/report.json"
rc=$?
set -e
[[ $rc -eq 75 ]]
[[ ! -e "$tmp/cases/$case_id.json" ]]
[[ -s "$tmp/rejected-pressure/$case_id/attempt-001/case.json" ]]
[[ -s "$tmp/rejected-pressure/$case_id/attempt-001/rejection.json" ]]
echo concurrency-runner-common-ok
