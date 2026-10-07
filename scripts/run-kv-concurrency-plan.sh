#!/usr/bin/env bash
set -u -o pipefail

PLAN=${1:?usage: run-kv-concurrency-plan.sh PLAN.json [RUN_ID]}
RUN_ID=${2:-"$(date -u +%Y%m%dT%H%M%SZ)-kv-concurrency-probe"}
ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
PROFILE=quick
# shellcheck source=concurrency-matrix-policy.sh
source "$ROOT/scripts/concurrency-matrix-policy.sh"
# shellcheck source=concurrency-runner-common.sh
source "$ROOT/scripts/concurrency-runner-common.sh"
CASE_TIMEOUT_S=$(concurrency_case_timeout_s "$PROFILE") || exit 2
PERSY_LOCK_TIMEOUT_MS=${PERSY_LOCK_TIMEOUT_MS:-250}
[[ "$PERSY_LOCK_TIMEOUT_MS" =~ ^[1-9][0-9]*$ ]] || { echo "invalid PERSY_LOCK_TIMEOUT_MS=$PERSY_LOCK_TIMEOUT_MS" >&2; exit 2; }
command -v timeout >/dev/null || { echo "GNU timeout is required" >&2; exit 2; }
[[ -s "$PLAN" ]] || { echo "plan not found: $PLAN" >&2; exit 2; }
PLAN=$(readlink -f "$PLAN")
PLAN_SHA=$(sha256sum "$PLAN" | awk '{print $1}')

RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,noise} "$DATA_DIR"
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((10 * 1024 * 1024 * 1024))
(( free_bytes >= min_free_bytes )) || { echo "refusing concurrency plan: free=$free_bytes required=$min_free_bytes" >&2; exit 75; }
[[ -s "$RUN_DIR/host-start.txt" ]] || "$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"

TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"; BUILD_PROFILE=external
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
else
  BIN="$TARGET_DIR/release/kvconcurrency"; BUILD_PROFILE=release
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features kv-all --bin kvconcurrency || exit $?
fi
if [[ -n "${BENCH_BIN_SHA256:-}" ]]; then
  BIN_SHA="$BENCH_BIN_SHA256"
  actual=$(sha256sum "$BIN" | awk '{print $1}')
  [[ "$actual" == "$BIN_SHA" ]] || { echo "BENCH_BIN SHA mismatch: $actual != $BIN_SHA" >&2; exit 2; }
else
  BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}')
fi

RUNNER_SHA=$(sha256sum "$ROOT/scripts/run-kv-concurrency-plan.sh" | awk '{print $1}')
NOISE_SHA=$(sha256sum "$ROOT/scripts/check-external-noise.py" | awk '{print $1}')
PRESSURE_SHA=$(sha256sum "$ROOT/scripts/scrub-short-trial-pressure.py" | awk '{print $1}')
CONCURRENCY_POLICY_SHA=$(sha256sum "$ROOT/scripts/concurrency-matrix-policy.sh" | awk '{print $1}')
HOST_NAME=$(hostname); MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FILESYSTEM=$(findmnt -n -o FSTYPE --target "$DATA_DIR"); SOURCE=$(findmnt -n -o SOURCE --target "$DATA_DIR")

PLAN_TSV="$RUN_DIR/plan.tsv.new"
python3 - "$PLAN" "$PLAN_TSV" <<'PY_PLAN'
import json,sys
from pathlib import Path
plan=json.loads(Path(sys.argv[1]).read_text())
if plan.get("concurrency_probe_plan_version") != 1:
    raise SystemExit(f"unsupported plan version: {plan.get('concurrency_probe_plan_version')!r}")
cases=plan.get("cases")
if not isinstance(cases,list) or len(cases) != int(plan.get("case_count",-1)):
    raise SystemExit("plan case_count mismatch")
required=("scenario","engine","durability","workload","clients","records","ops","trial")
seen=set(); families={}
with open(sys.argv[2],"w") as out:
    for i,item in enumerate(cases):
        missing=[key for key in required if key not in item]
        if missing: raise SystemExit(f"plan case {i} missing {missing}")
        vals=[str(item[k]) for k in required]
        if any("|" in v or "\n" in v for v in vals): raise SystemExit(f"invalid delimiter in case {i}")
        clients=int(item["clients"]); records=int(item["records"]); ops=int(item["ops"]); trial=int(item["trial"])
        if min(clients,records,ops,trial) < 1: raise SystemExit(f"non-positive numeric field in case {i}")
        if clients > ops: raise SystemExit(f"clients > ops in case {i}")
        key=(item["scenario"],item["engine"],item["durability"],item["workload"],clients)
        if key in seen: raise SystemExit(f"duplicate client group: {key}")
        seen.add(key)
        family=key[:-1]
        previous=families.setdefault(family,(records,ops))
        if previous != (records,ops): raise SystemExit(f"family total-work mismatch: {family}: {previous} != {(records,ops)}")
        if item["workload"] == "delete-burst" and ops > records: raise SystemExit(f"delete-burst ops > records in case {i}")
        out.write("|".join(vals)+"\n")
PY_PLAN

if [[ -s "$RUN_DIR/plan.tsv" ]]; then
  if ! cmp -s "$RUN_DIR/plan.tsv" "$PLAN_TSV"; then
    rm -f "$PLAN_TSV"; echo "refusing plan resume: materialized plan changed" >&2; exit 2
  fi
  rm -f "$PLAN_TSV"
else
  mv "$PLAN_TSV" "$RUN_DIR/plan.tsv"
fi
mapfile -t JOBS < "$RUN_DIR/plan.tsv"
TOTAL=${#JOBS[@]}
(( TOTAL > 0 )) || { echo "empty plan" >&2; exit 2; }

SUPPORT_NEW="$RUN_DIR/support.json.new"
python3 - "$SUPPORT_NEW" <<PY_SUPPORT
import json
json.dump({
 "lane":"kv-concurrency-probe","profile":"quick","case_count":$TOTAL,"expect_trials":1,
 "plan_path":"$PLAN","plan_sha256":"$PLAN_SHA","build_profile":"$BUILD_PROFILE","benchmark_binary_sha256":"$BIN_SHA",
 "runner_sha256":"$RUNNER_SHA","concurrency_policy_sha256":"$CONCURRENCY_POLICY_SHA","noise_guard_sha256":"$NOISE_SHA","short_pressure_guard_sha256":"$PRESSURE_SHA",
 "case_timeout_s":$CASE_TIMEOUT_S,"persy_lock_timeout_ms":$PERSY_LOCK_TIMEOUT_MS,"write_pattern":"append-explicit",
 "hostname":"$HOST_NAME","machine_id_sha256":"$MACHINE_ID_SHA256","filesystem":"$FILESYSTEM","source":"$SOURCE",
 "total_work_semantics":"each planned family keeps identical records and total ops across client counts"
},open("$SUPPORT_NEW","w"),sort_keys=True,separators=(",",":"))
PY_SUPPORT
EXISTING_CASES=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
if [[ -s "$RUN_DIR/support.json" ]]; then
  if ! cmp -s "$RUN_DIR/support.json" "$SUPPORT_NEW"; then
    if (( EXISTING_CASES > 0 )); then rm -f "$SUPPORT_NEW"; echo "refusing plan resume: support identity changed" >&2; exit 2; fi
    mv "$SUPPORT_NEW" "$RUN_DIR/support.json"
  else rm -f "$SUPPORT_NEW"; fi
else mv "$SUPPORT_NEW" "$RUN_DIR/support.json"; fi

RESUME_ORDER_POLICY=fixed-initial
[[ "${MATRIX_RESUME_SHUFFLE_REMAINING:-1}" == 1 ]] && RESUME_ORDER_POLICY=reshuffle-remaining
concurrency_prepare_order "$RUN_DIR" "$RESUME_ORDER_POLICY" "${JOBS[@]}" || exit $?

clear_failure() {
  local case_id=$1 failures="$RUN_DIR/failures.ndjson" tmp
  [[ -f "$failures" ]] || return 0
  tmp="${failures}.tmp"; jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' "$failures" > "$tmp"; mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}

INDEX=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1))
  IFS='|' read -r scenario engine durability workload clients records ops trial <<< "$job"
  scenario_tag=${scenario#concurrency-}
  case_id="t${trial}-${scenario_tag}-${engine}-${durability}-${workload}-c${clients}-n${records}-o${ops}"
  out="$RUN_DIR/cases/$case_id.json"; err="$RUN_DIR/stderr/$case_id.log"
  noise_before="$RUN_DIR/noise/$case_id.before.json"; noise_after="$RUN_DIR/noise/$case_id.after.json"
  if [[ -s "$out" ]]; then clear_failure "$case_id"; continue; fi
  echo "[$INDEX/$TOTAL] $case_id" >&2
  concurrency_check_io_quiet "$PROFILE" "before:$case_id" || exit $?
  concurrency_check_external_noise "$ROOT" "$PROFILE" "before:$case_id" "$noise_before" || exit $?
  rm -f "$out"
  cmd=("$BIN" --engine "$engine" --durability "$durability" --workload "$workload" --records "$records" --ops "$ops" --clients "$clients" --value-bytes 256 --value-pattern pseudo-random --key-bytes 8 --key-shape sequential --access-pattern auto --write-pattern append --txn-size 100 --scan-len 100 --warmup-reads 5000 --trial "$trial" --seed 1592606758 --scenario "$scenario" --root "$DATA_DIR" --output "$out")
  if [[ "$engine" == persy ]]; then
    DBBENCH_PERSY_LOCK_TIMEOUT_MS="$PERSY_LOCK_TIMEOUT_MS" timeout --signal=TERM --kill-after=5s "${CASE_TIMEOUT_S}s" "${cmd[@]}" 2>"$err"
  else
    timeout --signal=TERM --kill-after=5s "${CASE_TIMEOUT_S}s" "${cmd[@]}" 2>"$err"
  fi
  rc=$?
  io_rc=0; concurrency_check_io_quiet "$PROFILE" "after:$case_id" || io_rc=$?
  noise_rc=0; concurrency_check_external_noise "$ROOT" "$PROFILE" "after:$case_id" "$noise_after" || noise_rc=$?
  if (( io_rc != 0 || noise_rc != 0 )); then
    rm -f "$out"; clear_failure "$case_id"
    if (( io_rc != 0 )); then exit "$io_rc"; else exit "$noise_rc"; fi
  fi
  if (( rc != 0 )) || [[ ! -s "$out" ]] || [[ $(wc -l < "$out") -ne 1 ]]; then
    rm -f "$out"; clear_failure "$case_id"
    failure_kind=benchmark-error; (( rc == 124 || rc == 137 )) && failure_kind=case-timeout
    jq -cn --arg case_id "$case_id" --arg stderr "$err" --arg failure_kind "$failure_kind" --argjson rc "$rc" '{case_id:$case_id,returncode:$rc,failure_kind:$failure_kind,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
    continue
  fi
  pressure_report=$(mktemp)
  pressure_rc=0
  concurrency_scrub_case_pressure "$ROOT" "$PROFILE" "$RUN_DIR" "$case_id" "$pressure_report" || pressure_rc=$?
  rm -f "$pressure_report"
  if (( pressure_rc != 0 )); then clear_failure "$case_id"; exit "$pressure_rc"; fi
  clear_failure "$case_id"; [[ -s "$err" ]] || rm -f "$err"
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
if uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" --expect-trials 1 --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"; then summary_rc=0; else summary_rc=$?; fi
FAILURES=0; [[ -s "$RUN_DIR/failures.ndjson" ]] && FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson")
COMPLETED=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
printf 'run=%s total=%s completed=%s failures=%s summary_rc=%s results=%s\n' "$RUN_ID" "$TOTAL" "$COMPLETED" "$FAILURES" "$summary_rc" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 || summary_rc != 0 || COMPLETED != TOTAL ? 1 : 0 ))
