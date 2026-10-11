#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke) TRIALS=1; SETTLE_MS=500; SETTLE_SAMPLE_MS=100; MIN_FREE_GIB=5; CORE_PATTERNS=(append churn) ;;
  quick) TRIALS=3; SETTLE_MS=5000; SETTLE_SAMPLE_MS=250; MIN_FREE_GIB=20; CORE_PATTERNS=(append update-uniform update-hot95 churn) ;;
  full) TRIALS=5; SETTLE_MS=30000; SETTLE_SAMPLE_MS=500; MIN_FREE_GIB=50; CORE_PATTERNS=(append update-uniform update-hot95 churn) ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac
CASE_MIN_FREE_GIB=${PERFORMANCE_MIN_FREE_GIB:-$MIN_FREE_GIB}

ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
# shellcheck source=sustained-matrix-policy.sh
source "$ROOT/scripts/sustained-matrix-policy.sh"
# shellcheck source=performance-runner-common.sh
source "$ROOT/scripts/performance-runner-common.sh"
# shellcheck source=record-result-cleanup.sh
source "$ROOT/scripts/record-result-cleanup.sh"
read -r CORE_RECORDS CORE_OPS CORE_WINDOW CORE_PAYLOAD < <(record_sustained_budget "$PROFILE" core 100) || exit 2
read -r STRESS_RECORDS STRESS_OPS STRESS_WINDOW STRESS_PAYLOAD < <(record_sustained_budget "$PROFILE" stress 100) || exit 2
read -r RELAXED_RECORDS RELAXED_OPS RELAXED_WINDOW RELAXED_PAYLOAD < <(record_sustained_budget "$PROFILE" relaxed 100) || exit 2
read -r TX1_RECORDS TX1_OPS TX1_WINDOW TX1_PAYLOAD < <(record_sustained_budget "$PROFILE" txn 1) || exit 2
read -r TX1000_RECORDS TX1000_OPS TX1000_WINDOW TX1000_PAYLOAD < <(record_sustained_budget "$PROFILE" txn 1000) || exit 2

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-record-sustained-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,noise} "$DATA_DIR"
if [[ "${MATRIX_PLAN_ONLY:-0}" != 1 ]]; then
  free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
  min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
  (( free_bytes >= min_free_bytes )) || { echo "refusing record sustained run: free=$free_bytes required=$min_free_bytes" >&2; exit 75; }
  [[ -s "$RUN_DIR/host-start.txt" ]] || "$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
fi

TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
ROCKS_TARGET_DIR=${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}
ROCKS_MANIFEST="$ROOT/engines/surrealdb-rocksdb/Cargo.toml"
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"; BUILD_PROFILE=external
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
elif [[ "$PROFILE" == smoke ]]; then
  BIN="$TARGET_DIR/debug/recordsustained"; BUILD_PROFILE=debug
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --locked --features record --bin recordsustained || exit $?
else
  BIN="$TARGET_DIR/release/recordsustained"; BUILD_PROFILE=release
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features record --bin recordsustained || exit $?
fi
if [[ -n "${BENCH_ROCKS_BIN:-}" ]]; then
  ROCKS_BIN="$BENCH_ROCKS_BIN"; ROCKS_BUILD_PROFILE=external
  [[ -x "$ROCKS_BIN" ]] || { echo "BENCH_ROCKS_BIN is not executable: $ROCKS_BIN" >&2; exit 2; }
elif [[ "$PROFILE" == smoke ]]; then
  ROCKS_BIN="$ROCKS_TARGET_DIR/debug/surrealdb-rocksdb-recordsustained"; ROCKS_BUILD_PROFILE=debug
  CARGO_TARGET_DIR="$ROCKS_TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --locked --manifest-path "$ROCKS_MANIFEST" --bin surrealdb-rocksdb-recordsustained || exit $?
else
  ROCKS_BIN="$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordsustained"; ROCKS_BUILD_PROFILE=release
  CARGO_TARGET_DIR="$ROCKS_TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --manifest-path "$ROCKS_MANIFEST" --bin surrealdb-rocksdb-recordsustained || exit $?
fi
if [[ -n "${BENCH_BIN_SHA256:-}" ]]; then BIN_SHA="$BENCH_BIN_SHA256"; else BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}'); fi
if [[ -n "${BENCH_ROCKS_BIN_SHA256:-}" ]]; then ROCKS_BIN_SHA="$BENCH_ROCKS_BIN_SHA256"; else ROCKS_BIN_SHA=$(sha256sum "$ROCKS_BIN" | awk '{print $1}'); fi
HARNESS_COMMIT=$(git -C "$ROOT" rev-parse HEAD)
if [[ "$BUILD_PROFILE" == external ]]; then
  BENCH_SOURCE_COMMIT=${BENCH_SOURCE_COMMIT:-}
else
  BENCH_SOURCE_COMMIT=${BENCH_SOURCE_COMMIT:-$HARNESS_COMMIT}
fi
if [[ "$ROCKS_BUILD_PROFILE" == external ]]; then
  ROCKS_BENCH_SOURCE_COMMIT=${ROCKS_BENCH_SOURCE_COMMIT:-}
else
  ROCKS_BENCH_SOURCE_COMMIT=${ROCKS_BENCH_SOURCE_COMMIT:-$HARNESS_COMMIT}
fi
[[ "$BENCH_SOURCE_COMMIT" =~ ^[0-9a-f]{40}$ ]] || { echo "invalid or missing BENCH_SOURCE_COMMIT=$BENCH_SOURCE_COMMIT" >&2; exit 2; }
[[ "$ROCKS_BENCH_SOURCE_COMMIT" =~ ^[0-9a-f]{40}$ ]] || { echo "invalid or missing ROCKS_BENCH_SOURCE_COMMIT=$ROCKS_BENCH_SOURCE_COMMIT" >&2; exit 2; }
READ_MATERIALIZATION=full-record-v1
WRITE_MATERIALIZATION=no-return-v1
RUNNER_SHA=$(sha256sum "$ROOT/scripts/run-record-sustained-matrix.sh" | awk '{print $1}')
POLICY_SHA=$(sha256sum "$ROOT/scripts/sustained-matrix-policy.sh" | awk '{print $1}')
COMMON_SHA=$(sha256sum "$ROOT/scripts/performance-runner-common.sh" | awk '{print $1}')
NOISE_SHA=$(sha256sum "$ROOT/scripts/check-external-noise.py" | awk '{print $1}')
CONTINUOUS_NOISE_SHA=$(sha256sum "$ROOT/scripts/run-with-continuous-noise.py" | awk '{print $1}')
CONTINUOUS_NOISE_COMMON_SHA=$(sha256sum "$ROOT/scripts/continuous-noise-runner-common.sh" | awk '{print $1}')
HOST_NAME=$(hostname); MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FILESYSTEM=$(findmnt -n -o FSTYPE --target "$DATA_DIR"); SOURCE=$(findmnt -n -o SOURCE --target "$DATA_DIR")

ENGINES=(surrealdb surrealdb-rocksdb turso sqlite)
JOBS=()
add_job() { JOBS+=("$1|$2|$3|$4|$5|$6|$7|$8|$9|${10}"); }
clear_failure() {
  local case_id=$1 failures="$RUN_DIR/failures.ndjson" tmp
  [[ -f "$failures" ]] || return 0
  tmp="${failures}.tmp"; jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' "$failures" > "$tmp"; mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}
for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    for pattern in "${CORE_PATTERNS[@]}"; do add_job record-sustained-core "$engine" sync "$pattern" "$CORE_RECORDS" "$CORE_OPS" "$CORE_WINDOW" "$CORE_PAYLOAD" 100 "$trial"; done
    for pattern in append churn; do
      add_job record-sustained-4k-stress "$engine" sync "$pattern" "$STRESS_RECORDS" "$STRESS_OPS" "$STRESS_WINDOW" "$STRESS_PAYLOAD" 100 "$trial"
      add_job record-sustained-relaxed "$engine" relaxed "$pattern" "$RELAXED_RECORDS" "$RELAXED_OPS" "$RELAXED_WINDOW" "$RELAXED_PAYLOAD" 100 "$trial"
    done
    add_job record-sustained-txn-extremes "$engine" sync append "$TX1_RECORDS" "$TX1_OPS" "$TX1_WINDOW" "$TX1_PAYLOAD" 1 "$trial"
    add_job record-sustained-txn-extremes "$engine" sync append "$TX1000_RECORDS" "$TX1000_OPS" "$TX1000_WINDOW" "$TX1000_PAYLOAD" 1000 "$trial"
  done
done
TOTAL=${#JOBS[@]}
EXPECTED=$(record_sustained_expected_cases "$PROFILE") || exit 2
(( TOTAL == EXPECTED )) || { echo "record sustained case-count invariant failed: got=$TOTAL expected=$EXPECTED" >&2; exit 2; }
RESUME_ORDER_POLICY=fixed-initial; [[ "${MATRIX_RESUME_SHUFFLE_REMAINING:-0}" == 1 ]] && RESUME_ORDER_POLICY=reshuffle-remaining
SUPPORT_NEW="$RUN_DIR/support.json.new"
cat > "$SUPPORT_NEW" <<JSON
{"lane":"record-sustained","profile":"$PROFILE","trials":$TRIALS,"case_count":$TOTAL,"build_profile":"$BUILD_PROFILE","benchmark_binary_sha256":"$BIN_SHA","benchmark_source_commit":"$BENCH_SOURCE_COMMIT","rocks_build_profile":"$ROCKS_BUILD_PROFILE","rocksdb_benchmark_binary_sha256":"$ROCKS_BIN_SHA","surrealdb_rocksdb_source_commit":"$ROCKS_BENCH_SOURCE_COMMIT","harness_commit":"$HARNESS_COMMIT","read_materialization":"$READ_MATERIALIZATION","write_materialization":"$WRITE_MATERIALIZATION","runner_sha256":"$RUNNER_SHA","sustained_policy_sha256":"$POLICY_SHA","performance_common_sha256":"$COMMON_SHA","continuous_noise_common_sha256":"$CONTINUOUS_NOISE_COMMON_SHA","noise_guard_sha256":"$NOISE_SHA","continuous_noise_guard_sha256":"$CONTINUOUS_NOISE_SHA","admission_policy":"$PERFORMANCE_ADMISSION_POLICY","continuous_noise_sample_ms":$CONTINUOUS_NOISE_SAMPLE_MS,"continuous_noise_max_cpu_percent":$CONTINUOUS_NOISE_MAX_CPU_PERCENT,"continuous_noise_max_io_average_mib_s":$CONTINUOUS_NOISE_MAX_IO_AVERAGE_MIB_S,"continuous_noise_max_io_rate_mib_s":$CONTINUOUS_NOISE_MAX_IO_RATE_MIB_S,"initial_min_free_gib":$MIN_FREE_GIB,"case_min_free_gib":"$CASE_MIN_FREE_GIB","resume_order_policy":"$RESUME_ORDER_POLICY","hostname":"$HOST_NAME","machine_id_sha256":"$MACHINE_ID_SHA256","filesystem":"$FILESYSTEM","source":"$SOURCE","engines":"${ENGINES[*]}","core":{"records":$CORE_RECORDS,"ops":$CORE_OPS,"window_ops":$CORE_WINDOW,"payload_bytes":$CORE_PAYLOAD},"stress":{"records":$STRESS_RECORDS,"ops":$STRESS_OPS,"window_ops":$STRESS_WINDOW,"payload_bytes":$STRESS_PAYLOAD},"relaxed":{"records":$RELAXED_RECORDS,"ops":$RELAXED_OPS,"window_ops":$RELAXED_WINDOW,"payload_bytes":$RELAXED_PAYLOAD},"txn1":{"records":$TX1_RECORDS,"ops":$TX1_OPS,"window_ops":$TX1_WINDOW,"payload_bytes":$TX1_PAYLOAD},"txn1000":{"records":$TX1000_RECORDS,"ops":$TX1000_OPS,"window_ops":$TX1000_WINDOW,"payload_bytes":$TX1000_PAYLOAD},"window_method":"fixed logical-op windows; no recursive database-size scan between windows","churn_transaction_semantics":"40/30/30 update/insert/delete operations are shuffled and committed as one mixed transaction per batch","logical_mutated_bytes":"estimated logical record bytes: id(8)+bucket(4)+payload for upsert; id(8) for delete","threshold_interpretation":"75/50/25% baseline ratios are reporting diagnostics, not pass/fail criteria","baseline_method":"median throughput and p99 latency of the first min(3, window_count) windows","targeted_sweeps":["4KiB payload stress","relaxed durability","txn-size extremes 1 and 1000"],"backend_coverage":"SurrealDB/SurrealKV, SurrealDB/RocksDB, Turso, SQLite","post_workload_settle_ms":$SETTLE_MS}
JSON
EXISTING_CASES=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
if [[ -s "$RUN_DIR/support.json" ]]; then
  if ! cmp -s "$RUN_DIR/support.json" "$SUPPORT_NEW"; then
    if (( EXISTING_CASES > 0 )); then rm -f "$SUPPORT_NEW"; echo "refusing record sustained resume: support identity changed" >&2; exit 2; fi
    mv "$SUPPORT_NEW" "$RUN_DIR/support.json"
  else rm -f "$SUPPORT_NEW"; fi
else mv "$SUPPORT_NEW" "$RUN_DIR/support.json"; fi
performance_prepare_order "$RUN_DIR" "$RESUME_ORDER_POLICY" "${JOBS[@]}" || exit $?
if [[ "${MATRIX_PLAN_ONLY:-0}" == 1 ]]; then
  echo "plan-only run=$RUN_ID total=$TOTAL jobs=$RUN_DIR/jobs.txt support=$RUN_DIR/support.json"
  exit 0
fi
"$ROOT/scripts/ensure-sqlite-3.53.4.sh" || exit $?

INDEX=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1)); IFS='|' read -r scenario engine dur pattern records ops window payload txn trial <<< "$job"
  case_id="t${trial}-${scenario}-${engine}-${dur}-${pattern}-n${records}-o${ops}-w${window}-p${payload}-tx${txn}"
  out="$RUN_DIR/cases/$case_id.json"; err="$RUN_DIR/stderr/$case_id.log"
  noise_before="$RUN_DIR/noise/$case_id.before.json"; noise_during="$RUN_DIR/noise/$case_id.during.json"; noise_after="$RUN_DIR/noise/$case_id.after.json"
  if [[ -s "$out" ]]; then clear_failure "$case_id"; continue; fi
  echo "[$INDEX/$TOTAL] $case_id" >&2
  performance_check_io_quiet "$PROFILE" "before:$case_id" || exit $?
  performance_check_external_noise "$ROOT" "$PROFILE" "before:$case_id" "$noise_before" || exit $?
  rm -f "$out"
  CASE_BIN="$BIN"; [[ "$engine" == surrealdb-rocksdb ]] && CASE_BIN="$ROCKS_BIN"
  performance_run_with_continuous_noise "$ROOT" "$noise_during" "$CASE_BIN" --engine "$engine" --durability "$dur" --pattern "$pattern" --records "$records" --ops "$ops" --window-ops "$window" --payload-bytes "$payload" --txn-size "$txn" --trial "$trial" --seed 1592606758 --warmup-reads 5000 --settle-ms "$SETTLE_MS" --settle-sample-ms "$SETTLE_SAMPLE_MS" --scenario "$scenario" --root "$DATA_DIR" --output "$out" --keep-db 2>"$err"
  rc=$?
  noise_rc=0; performance_check_external_noise "$ROOT" "$PROFILE" "after:$case_id" "$noise_after" || noise_rc=$?
  if (( noise_rc != 0 )); then
    performance_preserve_noise_rejection "$RUN_DIR" "$case_id" post-external "$noise_before" "$noise_during" "$noise_after"
    if [[ -s "$out" ]]; then record_cleanup_result_db "$out" "$DATA_DIR" || true; fi
    rm -f "$out"; clear_failure "$case_id"
    exit "$noise_rc"
  fi
  if (( rc == 75 )) && performance_continuous_rejected "$noise_during"; then
    performance_preserve_noise_rejection "$RUN_DIR" "$case_id" continuous "$noise_before" "$noise_during" "$noise_after"
    if [[ -s "$out" ]]; then record_cleanup_result_db "$out" "$DATA_DIR" || true; fi
    rm -f "$out"; clear_failure "$case_id"
    exit 75
  fi
  if (( rc == 0 )) && [[ -s "$out" ]]; then
    record_cleanup_result_db "$out" "$DATA_DIR" || rc=$?
  fi
  if (( rc != 0 )) || [[ ! -s "$out" ]]; then
    rm -f "$out"; clear_failure "$case_id"
    jq -cn --arg case_id "$case_id" --arg stderr "$err" --argjson rc "$rc" '{case_id:$case_id,returncode:$rc,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
    continue
  fi
  clear_failure "$case_id"; [[ -s "$err" ]] || rm -f "$err"
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r jq -c . > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
if uv run --script "$ROOT/scripts/summarize-sustained.py" "$RUN_DIR" --expect-trials "$TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"; then summary_rc=0; else summary_rc=$?; fi
if [[ -s "$RUN_DIR/failures.ndjson" ]]; then FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson"); else FAILURES=0; fi
COMPLETED=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
echo "run=$RUN_ID total=$TOTAL completed=$COMPLETED failures=$FAILURES summary_rc=$summary_rc results=$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 || summary_rc != 0 || COMPLETED != TOTAL ? 1 : 0 ))
