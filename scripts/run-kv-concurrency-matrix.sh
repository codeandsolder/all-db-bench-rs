#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke)
    TRIALS=1; RECORDS=5000; OPS=3000; MIN_FREE_GIB=2
    CLIENTS=(1 4 8); CORE_WORKLOADS=(point-read read-heavy tiny-txn write-burst)
    RANGE_CLIENTS=(1 8); DELETE_CLIENTS=(1 8); RELAXED_CLIENTS=(1 8)
    ;;
  quick)
    TRIALS=3; RECORDS=100000; OPS=50000; MIN_FREE_GIB=10
    CLIENTS=(1 2 4 8); CORE_WORKLOADS=(point-read read-heavy balanced tiny-txn write-burst churn)
    RANGE_CLIENTS=(1 4 8); DELETE_CLIENTS=(1 4 8); RELAXED_CLIENTS=(1 4 8)
    ;;
  full)
    TRIALS=7; RECORDS=1000000; OPS=250000; MIN_FREE_GIB=30
    CLIENTS=(1 2 4 8 16); CORE_WORKLOADS=(point-read read-heavy balanced tiny-txn write-burst churn)
    RANGE_CLIENTS=(1 2 4 8 16); DELETE_CLIENTS=(1 2 4 8 16); RELAXED_CLIENTS=(1 2 4 8 16)
    ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac

ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
# shellcheck source=kv-matrix-policy.sh
source "$ROOT/scripts/kv-matrix-policy.sh"
# shellcheck source=concurrency-matrix-policy.sh
source "$ROOT/scripts/concurrency-matrix-policy.sh"
# shellcheck source=concurrency-runner-common.sh
source "$ROOT/scripts/concurrency-runner-common.sh"
CASE_TIMEOUT_S=$(concurrency_case_timeout_s "$PROFILE") || exit 2
PERSY_LOCK_TIMEOUT_MS=${PERSY_LOCK_TIMEOUT_MS:-250}
[[ "$PERSY_LOCK_TIMEOUT_MS" =~ ^[1-9][0-9]*$ ]] || { echo "invalid PERSY_LOCK_TIMEOUT_MS=$PERSY_LOCK_TIMEOUT_MS" >&2; exit 2; }
command -v timeout >/dev/null || { echo "GNU timeout is required" >&2; exit 2; }

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-kv-concurrency-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"; DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,noise} "$DATA_DIR"
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
(( free_bytes >= min_free_bytes )) || { echo "refusing KV concurrency run: free=$free_bytes required=$min_free_bytes" >&2; exit 75; }
[[ -s "$RUN_DIR/host-start.txt" ]] || "$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"

TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"; BUILD_PROFILE=external
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
elif [[ "$PROFILE" == smoke ]]; then
  BIN="$TARGET_DIR/debug/kvconcurrency"; BUILD_PROFILE=debug
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --locked --features kv-all --bin kvconcurrency || exit $?
else
  BIN="$TARGET_DIR/release/kvconcurrency"; BUILD_PROFILE=release
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features kv-all --bin kvconcurrency || exit $?
fi
if [[ -n "${BENCH_BIN_SHA256:-}" ]]; then BIN_SHA="$BENCH_BIN_SHA256"; else BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}'); fi
RUNNER_SHA=$(sha256sum "$ROOT/scripts/run-kv-concurrency-matrix.sh" | awk '{print $1}')
KV_POLICY_SHA=$(sha256sum "$ROOT/scripts/kv-matrix-policy.sh" | awk '{print $1}')
CONCURRENCY_POLICY_SHA=$(sha256sum "$ROOT/scripts/concurrency-matrix-policy.sh" | awk '{print $1}')
NOISE_SHA=$(sha256sum "$ROOT/scripts/check-external-noise.py" | awk '{print $1}')
PRESSURE_SHA=$(sha256sum "$ROOT/scripts/scrub-short-trial-pressure.py" | awk '{print $1}')
HOST_NAME=$(hostname); MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FILESYSTEM=$(findmnt -n -o FSTYPE --target "$DATA_DIR"); SOURCE=$(findmnt -n -o SOURCE --target "$DATA_DIR")
IMPORT_MANIFEST="$RUN_DIR/import-manifest.json"
IMPORT_MANIFEST_SHA=""
if [[ -s "$IMPORT_MANIFEST" ]]; then
  IMPORT_MANIFEST_SHA=$(sha256sum "$IMPORT_MANIFEST" | awk '{print $1}')
  uv run --script "$ROOT/scripts/import-concurrency-results.py" --verify-run "$RUN_DIR" >/dev/null || exit 2
fi

ENGINES=(redb fjall surrealkv heed sled manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
[[ -n "${ENGINES_OVERRIDE:-}" ]] && read -r -a ENGINES <<< "$ENGINES_OVERRIDE"
ORDERED_ENGINES=(); for engine in "${ENGINES[@]}"; do [[ "$engine" == paritydb-hash ]] || ORDERED_ENGINES+=("$engine"); done
JOBS=()
primary_durability() { case "$1" in paritydb-hash|paritydb-btree) echo relaxed ;; *) echo sync ;; esac; }
relaxed_supported() { case "$1" in manifold|jammdb|lsmdb) return 1 ;; *) return 0 ;; esac; }
add_job() { JOBS+=("$1|$2|$3|$4|$5|$6"); }
clear_failure() {
  local case_id=$1 failures="$RUN_DIR/failures.ndjson" tmp
  [[ -f "$failures" ]] || return 0
  tmp="${failures}.tmp"; jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' "$failures" > "$tmp"; mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}

for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    dur=$(primary_durability "$engine")
    for workload in "${CORE_WORKLOADS[@]}"; do for clients in "${CLIENTS[@]}"; do add_job primary "$engine" "$dur" "$workload" "$clients" "$trial"; done; done
  done
  for engine in "${ORDERED_ENGINES[@]}"; do
    dur=$(primary_durability "$engine"); for clients in "${RANGE_CLIENTS[@]}"; do add_job range "$engine" "$dur" range-scan "$clients" "$trial"; done
  done
  for engine in "${ENGINES[@]}"; do
    dur=$(primary_durability "$engine"); for clients in "${DELETE_CLIENTS[@]}"; do add_job delete "$engine" "$dur" delete-burst "$clients" "$trial"; done
  done
  for engine in "${ENGINES[@]}"; do
    [[ "$(primary_durability "$engine")" == relaxed ]] && continue
    relaxed_supported "$engine" || continue
    for workload in read-heavy write-burst; do for clients in "${RELAXED_CLIENTS[@]}"; do add_job relaxed "$engine" relaxed "$workload" "$clients" "$trial"; done; done
  done
done

RESUME_ORDER_POLICY=fixed-initial; [[ "${MATRIX_RESUME_SHUFFLE_REMAINING:-0}" == 1 ]] && RESUME_ORDER_POLICY=reshuffle-remaining
TOTAL=${#JOBS[@]}
SUPPORT_NEW="$RUN_DIR/support.json.new"
python3 - "$SUPPORT_NEW" <<PY_SUPPORT
import json,sys
json.dump({
 "lane":"kv-concurrency","profile":"$PROFILE","trials":$TRIALS,"records":$RECORDS,"default_ops":$OPS,
 "case_count":$TOTAL,"shared_database":True,"total_ops_fixed_across_client_counts":True,
 "engines":"${ENGINES[*]}","clients":"${CLIENTS[*]}","core_workloads":"${CORE_WORKLOADS[*]}",
 "range_clients":"${RANGE_CLIENTS[*]}","delete_clients":"${DELETE_CLIENTS[*]}","relaxed_clients":"${RELAXED_CLIENTS[*]}",
 "resume_order_policy":"$RESUME_ORDER_POLICY","build_profile":"$BUILD_PROFILE","benchmark_binary_sha256":"$BIN_SHA",
 "runner_sha256":"$RUNNER_SHA","kv_matrix_policy_sha256":"$KV_POLICY_SHA","concurrency_policy_sha256":"$CONCURRENCY_POLICY_SHA",
 "noise_guard_sha256":"$NOISE_SHA","short_pressure_guard_sha256":"$PRESSURE_SHA",
 "case_timeout_s":$CASE_TIMEOUT_S,"persy_lock_timeout_ms":$PERSY_LOCK_TIMEOUT_MS,
 "state_evolution":"growth","write_pattern":"append",
 "persy_timeout_retry_policy":"typed PrepareError::TransactionTimeout only; max 100 retries; deterministic micro-backoff; retry cost is timed",
 "import_manifest_sha256":("$IMPORT_MANIFEST_SHA" or None),
 "hostname":"$HOST_NAME","machine_id_sha256":"$MACHINE_ID_SHA256","filesystem":"$FILESYSTEM","source":"$SOURCE",
 "unsupported":[
   {"engine":"lkv","scope":"all concurrency workloads","reason":"no native clone/shared writer handle"},
   {"engine":"paritydb-hash","scope":"range-scan","reason":"hash-column mode is unordered"}
 ]
},open(sys.argv[1],"w"),sort_keys=True,separators=(",",":"))
PY_SUPPORT
EXISTING_CASES=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
if [[ -s "$RUN_DIR/support.json" ]]; then
  if ! cmp -s "$RUN_DIR/support.json" "$SUPPORT_NEW"; then
    if (( EXISTING_CASES > 0 )); then rm -f "$SUPPORT_NEW"; echo "refusing KV concurrency resume: support identity changed" >&2; exit 2; fi
    mv "$SUPPORT_NEW" "$RUN_DIR/support.json"
  else rm -f "$SUPPORT_NEW"; fi
else mv "$SUPPORT_NEW" "$RUN_DIR/support.json"; fi
concurrency_prepare_order "$RUN_DIR" "$RESUME_ORDER_POLICY" "${JOBS[@]}" || exit $?

INDEX=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1)); IFS='|' read -r scenario engine durability workload clients trial <<< "$job"
  scan=100; ops=$(kv_concurrency_ops "$PROFILE" "$workload" "$OPS" "$scan" "$RECORDS") || exit 2
  if [[ "$workload" == range-scan ]]; then
    (( ops < clients )) && ops=$clients; ops=$(kv_effective_ops "$engine" "$workload" "$ops" "$RECORDS") || exit 2
  elif [[ "$workload" == tiny-txn ]]; then
    (( ops < clients )) && ops=$clients
  elif [[ "$workload" == delete-burst && "$ops" -gt "$RECORDS" ]]; then ops=$RECORDS; fi
  case_id="t${trial}-${scenario}-${engine}-${durability}-${workload}-c${clients}-n${RECORDS}-o${ops}"
  out="$RUN_DIR/cases/$case_id.json"; err="$RUN_DIR/stderr/$case_id.log"
  noise_before="$RUN_DIR/noise/$case_id.before.json"; noise_after="$RUN_DIR/noise/$case_id.after.json"
  if [[ -s "$out" ]]; then clear_failure "$case_id"; continue; fi
  echo "[$INDEX/$TOTAL] $case_id" >&2
  concurrency_check_io_quiet "$PROFILE" "before:$case_id" || exit $?
  concurrency_check_external_noise "$ROOT" "$PROFILE" "before:$case_id" "$noise_before" || exit $?
  rm -f "$out"
  if [[ "$engine" == persy ]]; then
    DBBENCH_PERSY_LOCK_TIMEOUT_MS="$PERSY_LOCK_TIMEOUT_MS" timeout --signal=TERM --kill-after=5s "${CASE_TIMEOUT_S}s" \
      "$BIN" --engine "$engine" --durability "$durability" --workload "$workload" \
      --records "$RECORDS" --ops "$ops" --clients "$clients" --value-bytes 256 --value-pattern pseudo-random \
      --key-bytes 8 --key-shape sequential --access-pattern auto --write-pattern append --state-evolution growth --bounded-churn-slots 0 --txn-size 100 --scan-len "$scan" --warmup-reads 5000 \
      --trial "$trial" --seed 1592606758 --scenario "concurrency-$scenario" --root "$DATA_DIR" --output "$out" 2>"$err"
  else
    timeout --signal=TERM --kill-after=5s "${CASE_TIMEOUT_S}s" \
      "$BIN" --engine "$engine" --durability "$durability" --workload "$workload" \
      --records "$RECORDS" --ops "$ops" --clients "$clients" --value-bytes 256 --value-pattern pseudo-random \
      --key-bytes 8 --key-shape sequential --access-pattern auto --write-pattern append --state-evolution growth --bounded-churn-slots 0 --txn-size 100 --scan-len "$scan" --warmup-reads 5000 \
      --trial "$trial" --seed 1592606758 --scenario "concurrency-$scenario" --root "$DATA_DIR" --output "$out" 2>"$err"
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
  clear_failure "$case_id"; [[ -s "$err" ]] || rm -f "$err"
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
if uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" --expect-trials "$TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"; then summary_rc=0; else summary_rc=$?; fi
if [[ -s "$RUN_DIR/failures.ndjson" ]]; then FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson"); else FAILURES=0; fi
COMPLETED=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
printf 'run=%s total=%s completed=%s failures=%s summary_rc=%s results=%s\n' "$RUN_ID" "$TOTAL" "$COMPLETED" "$FAILURES" "$summary_rc" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 || summary_rc != 0 || COMPLETED != TOTAL ? 1 : 0 ))
