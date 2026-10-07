#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$ROOT/scripts/performance-runner-common.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/order" "$tmp/run/cases" "$tmp/run/noise" "$tmp/run/stderr"

performance_prepare_order "$tmp/order" fixed-initial "a" "b" "c"
[[ $(wc -l < "$tmp/order/jobs.txt") -eq 3 ]]
performance_prepare_order "$tmp/order" reshuffle-remaining "a" "b" "c"
set +e
performance_prepare_order "$tmp/order" fixed-initial "a" "b" "d"
order_rc=$?
set -e
[[ $order_rc -eq 2 ]]

case_id=t1-test-rocksdb-relaxed-write-burst
python3 - "$tmp/run/cases/$case_id.json" <<'PY'
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
performance_scrub_case_pressure "$ROOT" quick "$tmp/run" "$case_id" "$tmp/report.json"
pressure_rc=$?
set -e
[[ $pressure_rc -eq 75 ]]
[[ ! -e "$tmp/run/cases/$case_id.json" ]]
[[ -s "$tmp/run/rejected-pressure/$case_id/attempt-001/case.json" ]]
[[ -s "$tmp/run/rejected-pressure/$case_id/attempt-001/rejection.json" ]]
echo performance-runner-common-ok
