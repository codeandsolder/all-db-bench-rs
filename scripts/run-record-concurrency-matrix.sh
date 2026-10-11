#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke)
    TRIALS=1; RECORDS=2000; OPS=4000; PAYLOAD=128; TXN=100; MIN_FREE_GIB=2
    CLIENTS=(1 4 8); WORKLOADS=(point-read read-heavy write-burst)
    RELAXED_CLIENTS=(1 8); STRESS_CLIENTS=(1 8); TX_CLIENTS=(1 8)
    STRESS_PAYLOAD=2048; TX1_OPS=2000; TX1000_OPS=8000; HOT_RECORDS=64; HOT_OPS=4000
    ;;
  quick)
    TRIALS=3; RECORDS=10000; OPS=24000; PAYLOAD=512; TXN=100; MIN_FREE_GIB=10
    CLIENTS=(1 2 4 8); WORKLOADS=(point-read indexed-read read-heavy tiny-txn write-burst)
    RELAXED_CLIENTS=(1 4 8); STRESS_CLIENTS=(1 4 8); TX_CLIENTS=(1 4 8)
    STRESS_PAYLOAD=4096; TX1_OPS=24000; TX1000_OPS=24000; HOT_RECORDS=64; HOT_OPS=24000
    ;;
  full)
    TRIALS=5; RECORDS=100000; OPS=80000; PAYLOAD=512; TXN=100; MIN_FREE_GIB=30
    CLIENTS=(1 2 4 8 16); WORKLOADS=(point-read indexed-read read-heavy tiny-txn write-burst)
    RELAXED_CLIENTS=(1 4 8 16); STRESS_CLIENTS=(1 4 8 16); TX_CLIENTS=(1 4 8 16)
    STRESS_PAYLOAD=16384; TX1_OPS=80000; TX1000_OPS=80000; HOT_RECORDS=64; HOT_OPS=80000
    ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac
CASE_MIN_FREE_GIB=${PERFORMANCE_MIN_FREE_GIB:-$MIN_FREE_GIB}

ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
# shellcheck source=concurrency-matrix-policy.sh
source "$ROOT/scripts/concurrency-matrix-policy.sh"
# shellcheck source=concurrency-runner-common.sh
source "$ROOT/scripts/concurrency-runner-common.sh"
# shellcheck source=record-result-cleanup.sh
source "$ROOT/scripts/record-result-cleanup.sh"
CASE_TIMEOUT_S=$(concurrency_case_timeout_s "$PROFILE") || exit 2
command -v timeout >/dev/null || { echo "GNU timeout is required" >&2; exit 2; }
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-record-concurrency-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"; DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,noise} "$DATA_DIR"
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
(( free_bytes >= min_free_bytes )) || { echo "refusing record concurrency run: free=$free_bytes required=$min_free_bytes" >&2; exit 75; }
"$ROOT/scripts/ensure-sqlite-3.53.4.sh"
[[ -s "$RUN_DIR/host-start.txt" ]] || "$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"

TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"; BUILD_PROFILE=external
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
elif [[ "$PROFILE" == smoke ]]; then
  BIN="$TARGET_DIR/debug/recordconcurrency"; BUILD_PROFILE=debug
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --locked --features record --bin recordconcurrency || exit $?
else
  BIN="$TARGET_DIR/release/recordconcurrency"; BUILD_PROFILE=release
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features record --bin recordconcurrency || exit $?
fi
if [[ -n "${BENCH_BIN_SHA256:-}" ]]; then BIN_SHA="$BENCH_BIN_SHA256"; else BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}'); fi
ROCKS_TARGET_DIR=${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}
if [[ -n "${ROCKS_BENCH_BIN:-}" ]]; then
  ROCKS_BIN="$ROCKS_BENCH_BIN"; ROCKS_BUILD_PROFILE=external
  [[ -x "$ROCKS_BIN" ]] || { echo "ROCKS_BENCH_BIN is not executable: $ROCKS_BIN" >&2; exit 2; }
elif [[ "$PROFILE" == smoke ]]; then
  ROCKS_BIN="$ROCKS_TARGET_DIR/debug/surrealdb-rocksdb-recordconcurrency"; ROCKS_BUILD_PROFILE=debug
  "$ROOT/scripts/cargo-local-1.99.sh" build --locked --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" --target-dir "$ROCKS_TARGET_DIR" --bin surrealdb-rocksdb-recordconcurrency || exit $?
else
  ROCKS_BIN="$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordconcurrency"; ROCKS_BUILD_PROFILE=release
  "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" --target-dir "$ROCKS_TARGET_DIR" --bin surrealdb-rocksdb-recordconcurrency || exit $?
fi
if [[ -n "${ROCKS_BENCH_BIN_SHA256:-}" ]]; then ROCKS_BIN_SHA="$ROCKS_BENCH_BIN_SHA256"; else ROCKS_BIN_SHA=$(sha256sum "$ROCKS_BIN" | awk '{print $1}'); fi
RUNNER_SHA=$(sha256sum "$ROOT/scripts/run-record-concurrency-matrix.sh" | awk '{print $1}')
CONCURRENCY_POLICY_SHA=$(sha256sum "$ROOT/scripts/concurrency-matrix-policy.sh" | awk '{print $1}')
NOISE_SHA=$(sha256sum "$ROOT/scripts/check-external-noise.py" | awk '{print $1}')
CONTINUOUS_NOISE_SHA=$(sha256sum "$ROOT/scripts/run-with-continuous-noise.py" | awk '{print $1}')
CONCURRENCY_RUNNER_COMMON_SHA=$(sha256sum "$ROOT/scripts/concurrency-runner-common.sh" | awk '{print $1}')
CONTINUOUS_NOISE_COMMON_SHA=$(sha256sum "$ROOT/scripts/continuous-noise-runner-common.sh" | awk '{print $1}')
HOST_NAME=$(hostname); MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FILESYSTEM=$(findmnt -n -o FSTYPE --target "$DATA_DIR"); SOURCE=$(findmnt -n -o SOURCE --target "$DATA_DIR")

ENGINES=(surrealdb turso sqlite surrealdb-rocksdb)
[[ -n "${ENGINES_OVERRIDE:-}" ]] && read -r -a ENGINES <<< "$ENGINES_OVERRIDE"
JOBS=(); add() { JOBS+=("$1|$2|$3|$4|$5|$6|$7|$8|$9|${10}"); }
clear_failure() {
  local case_id=$1 failures="$RUN_DIR/failures.ndjson" tmp
  [[ -f "$failures" ]] || return 0
  tmp="${failures}.tmp"; jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' "$failures" > "$tmp"; mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}
for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    for clients in "${CLIENTS[@]}"; do
      for workload in "${WORKLOADS[@]}"; do
        case_ops=$(record_concurrency_ops "$PROFILE" record-concurrency-core "$workload" "$OPS") || exit 2
        add record-concurrency-core "$engine" sync "$workload" "$clients" "$RECORDS" "$case_ops" "$PAYLOAD" "$TXN" "$trial"
      done
    done
    for clients in "${RELAXED_CLIENTS[@]}"; do
      for workload in read-heavy write-burst; do
        case_ops=$(record_concurrency_ops "$PROFILE" record-concurrency-relaxed "$workload" "$OPS") || exit 2
        add record-concurrency-relaxed "$engine" relaxed "$workload" "$clients" "$RECORDS" "$case_ops" "$PAYLOAD" "$TXN" "$trial"
      done
    done
    for clients in "${STRESS_CLIENTS[@]}"; do
      for workload in read-heavy write-burst; do
        case_ops=$(record_concurrency_ops "$PROFILE" record-concurrency-large-payload "$workload" "$OPS") || exit 2
        add record-concurrency-large-payload "$engine" sync "$workload" "$clients" "$RECORDS" "$case_ops" "$STRESS_PAYLOAD" "$TXN" "$trial"
      done
    done
    for clients in "${STRESS_CLIENTS[@]}"; do
      case_ops=$(record_concurrency_ops "$PROFILE" record-concurrency-hotset read-heavy "$HOT_OPS") || exit 2
      add record-concurrency-hotset "$engine" sync read-heavy "$clients" "$HOT_RECORDS" "$case_ops" "$PAYLOAD" "$TXN" "$trial"
    done
    for clients in "${TX_CLIENTS[@]}"; do
      case_ops=$(record_concurrency_ops "$PROFILE" record-concurrency-txn-1 write-burst "$TX1_OPS") || exit 2
      add record-concurrency-txn-1 "$engine" sync write-burst "$clients" "$RECORDS" "$case_ops" "$PAYLOAD" 1 "$trial"
      case_ops=$(record_concurrency_ops "$PROFILE" record-concurrency-txn-1000 write-burst "$TX1000_OPS") || exit 2
      add record-concurrency-txn-1000 "$engine" sync write-burst "$clients" "$RECORDS" "$case_ops" "$PAYLOAD" 1000 "$trial"
    done
  done
done

RESUME_ORDER_POLICY=fixed-initial; [[ "${MATRIX_RESUME_SHUFFLE_REMAINING:-0}" == 1 ]] && RESUME_ORDER_POLICY=reshuffle-remaining
TOTAL=${#JOBS[@]}
SUPPORT_NEW="$RUN_DIR/support.json.new"
python3 - "$SUPPORT_NEW" <<PY_SUPPORT
import json,sys
json.dump({
 "lane":"record-concurrency","profile":"$PROFILE","trials":$TRIALS,"records":$RECORDS,"default_ops":$OPS,"payload_bytes":$PAYLOAD,"txn_size":$TXN,
 "case_count":$TOTAL,"engines":"${ENGINES[*]}","clients":"${CLIENTS[*]}","workloads":"${WORKLOADS[*]}",
 "relaxed_clients":"${RELAXED_CLIENTS[*]}","stress_clients":"${STRESS_CLIENTS[*]}","tx_clients":"${TX_CLIENTS[*]}",
 "stress_payload":$STRESS_PAYLOAD,"hot_records":$HOT_RECORDS,"resume_order_policy":"$RESUME_ORDER_POLICY",
 "build_profile":"$BUILD_PROFILE","benchmark_binary_sha256":"$BIN_SHA","rocks_build_profile":"$ROCKS_BUILD_PROFILE","surrealdb_rocksdb_binary_sha256":"$ROCKS_BIN_SHA",
 "runner_sha256":"$RUNNER_SHA","concurrency_runner_common_sha256":"$CONCURRENCY_RUNNER_COMMON_SHA","continuous_noise_common_sha256":"$CONTINUOUS_NOISE_COMMON_SHA","concurrency_policy_sha256":"$CONCURRENCY_POLICY_SHA","noise_guard_sha256":"$NOISE_SHA","continuous_noise_guard_sha256":"$CONTINUOUS_NOISE_SHA","admission_policy":"$CONCURRENCY_ADMISSION_POLICY","continuous_noise_sample_ms":$CONTINUOUS_NOISE_SAMPLE_MS,"continuous_noise_max_cpu_percent":$CONTINUOUS_NOISE_MAX_CPU_PERCENT,"continuous_noise_max_io_average_mib_s":$CONTINUOUS_NOISE_MAX_IO_AVERAGE_MIB_S,"continuous_noise_max_io_rate_mib_s":$CONTINUOUS_NOISE_MAX_IO_RATE_MIB_S,"initial_min_free_gib":$MIN_FREE_GIB,"case_min_free_gib":"$CASE_MIN_FREE_GIB","case_timeout_s":$CASE_TIMEOUT_S,
 "hostname":"$HOST_NAME","machine_id_sha256":"$MACHINE_ID_SHA256","filesystem":"$FILESYSTEM","source":"$SOURCE",
 "total_work_semantics":"ops is total logical work across all clients; it is not multiplied by client count",
 "writer_semantics":"write-burst partitions whole transactions only; clients never receive a benchmark-manufactured partial transaction",
 "surreal_conflict_policy":"retry only typed transaction conflicts, bounded at 10000; retry time stays inside measured latency"
},open(sys.argv[1],"w"),sort_keys=True,separators=(",",":"))
PY_SUPPORT
EXISTING_CASES=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
if [[ -s "$RUN_DIR/support.json" ]]; then
  if ! cmp -s "$RUN_DIR/support.json" "$SUPPORT_NEW"; then
    if (( EXISTING_CASES > 0 )); then rm -f "$SUPPORT_NEW"; echo "refusing record concurrency resume: support identity changed" >&2; exit 2; fi
    mv "$SUPPORT_NEW" "$RUN_DIR/support.json"
  else rm -f "$SUPPORT_NEW"; fi
else mv "$SUPPORT_NEW" "$RUN_DIR/support.json"; fi
concurrency_prepare_order "$RUN_DIR" "$RESUME_ORDER_POLICY" "${JOBS[@]}" || exit $?

INDEX=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1)); IFS='|' read -r scenario engine dur workload clients records ops payload txn trial <<< "$job"
  case_id="t${trial}-${scenario}-${engine}-${dur}-${workload}-c${clients}-n${records}-o${ops}-p${payload}-tx${txn}"
  out="$RUN_DIR/cases/$case_id.json"; err="$RUN_DIR/stderr/$case_id.log"
  noise_before="$RUN_DIR/noise/$case_id.before.json"; noise_during="$RUN_DIR/noise/$case_id.during.json"; noise_after="$RUN_DIR/noise/$case_id.after.json"
  if [[ -s "$out" ]]; then clear_failure "$case_id"; continue; fi
  echo "[$INDEX/$TOTAL] $case_id" >&2
  concurrency_check_io_quiet "$PROFILE" "before:$case_id" || exit $?
  concurrency_check_external_noise "$ROOT" "$PROFILE" "before:$case_id" "$noise_before" || exit $?
  CASE_BIN="$BIN"; [[ "$engine" == surrealdb-rocksdb ]] && CASE_BIN="$ROCKS_BIN"
  rm -f "$out"
  concurrency_run_with_continuous_noise "$ROOT" "$noise_during" \
    timeout --signal=TERM --kill-after=5s "${CASE_TIMEOUT_S}s" "$CASE_BIN" --engine "$engine" --durability "$dur" --workload "$workload" --clients "$clients" --records "$records" --ops "$ops" \
    --payload-bytes "$payload" --txn-size "$txn" --trial "$trial" --seed 1592606758 --scenario "$scenario" --warmup-reads 5000 --root "$DATA_DIR" --output "$out" --keep-db 2>"$err"
  rc=$?
  noise_rc=0; concurrency_check_external_noise "$ROOT" "$PROFILE" "after:$case_id" "$noise_after" || noise_rc=$?
  if (( noise_rc != 0 )); then
    concurrency_preserve_noise_rejection "$RUN_DIR" "$case_id" post-external "$noise_before" "$noise_during" "$noise_after"
    if [[ -s "$out" ]]; then record_cleanup_result_db "$out" "$DATA_DIR" || true; fi
    rm -f "$out"; clear_failure "$case_id"
    exit "$noise_rc"
  fi
  if (( rc == 75 )) && concurrency_continuous_rejected "$noise_during"; then
    concurrency_preserve_noise_rejection "$RUN_DIR" "$case_id" continuous "$noise_before" "$noise_during" "$noise_after"
    if [[ -s "$out" ]]; then record_cleanup_result_db "$out" "$DATA_DIR" || true; fi
    rm -f "$out"; clear_failure "$case_id"
    exit 75
  fi
  if (( rc == 0 )) && [[ -s "$out" ]] && [[ $(wc -l < "$out") -eq 1 ]]; then
    record_cleanup_result_db "$out" "$DATA_DIR" || rc=$?
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
