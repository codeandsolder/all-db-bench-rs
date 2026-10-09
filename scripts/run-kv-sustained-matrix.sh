#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke) TRIALS=1; SETTLE_MS=500; SETTLE_SAMPLE_MS=100; MIN_FREE_GIB=5; CORE_PATTERNS=(append churn) ;;
  quick) TRIALS=3; SETTLE_MS=5000; SETTLE_SAMPLE_MS=250; MIN_FREE_GIB=20; CORE_PATTERNS=(append update-uniform update-hot95 churn) ;;
  full) TRIALS=5; SETTLE_MS=30000; SETTLE_SAMPLE_MS=500; MIN_FREE_GIB=50; CORE_PATTERNS=(append update-uniform update-hot95 churn) ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
# shellcheck source=sustained-matrix-policy.sh
source "$ROOT/scripts/sustained-matrix-policy.sh"
# shellcheck source=performance-runner-common.sh
source "$ROOT/scripts/performance-runner-common.sh"
read -r CORE_RECORDS CORE_OPS CORE_WINDOW CORE_VALUE < <(kv_sustained_budget "$PROFILE" core 100) || exit 2
read -r STRESS_RECORDS STRESS_OPS STRESS_WINDOW STRESS_VALUE < <(kv_sustained_budget "$PROFILE" stress 100) || exit 2
read -r RELAXED_RECORDS RELAXED_OPS RELAXED_WINDOW RELAXED_VALUE < <(kv_sustained_budget "$PROFILE" relaxed 100) || exit 2
read -r TX1_RECORDS TX1_OPS TX1_WINDOW TX1_VALUE < <(kv_sustained_budget "$PROFILE" txn 1) || exit 2
read -r TX1000_RECORDS TX1000_OPS TX1000_WINDOW TX1000_VALUE < <(kv_sustained_budget "$PROFILE" txn 1000) || exit 2

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-kv-sustained-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,noise} "$DATA_DIR"
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
(( free_bytes >= min_free_bytes )) || { echo "refusing sustained run: free=$free_bytes required=$min_free_bytes" >&2; exit 75; }
[[ -s "$RUN_DIR/host-start.txt" ]] || "$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"

TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"; BUILD_PROFILE=external
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
else
  BIN="$TARGET_DIR/release/kvsustained"; BUILD_PROFILE=release
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --features kv-all --bin kvsustained || exit $?
fi
if [[ -n "${BENCH_BIN_SHA256:-}" ]]; then BIN_SHA="$BENCH_BIN_SHA256"; else BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}'); fi
RUNNER_SHA=$(sha256sum "$ROOT/scripts/run-kv-sustained-matrix.sh" | awk '{print $1}')
POLICY_SHA=$(sha256sum "$ROOT/scripts/sustained-matrix-policy.sh" | awk '{print $1}')
COMMON_SHA=$(sha256sum "$ROOT/scripts/performance-runner-common.sh" | awk '{print $1}')
NOISE_SHA=$(sha256sum "$ROOT/scripts/check-external-noise.py" | awk '{print $1}')
HOST_NAME=$(hostname); MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FILESYSTEM=$(findmnt -n -o FSTYPE --target "$DATA_DIR"); SOURCE=$(findmnt -n -o SOURCE --target "$DATA_DIR")

ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
STRESS_ENGINES=(fjall surrealkv sled turbokv rocksdb roughdb lsmdb)
RELAXED_ENGINES=(redb fjall surrealkv heed sled turbokv rocksdb mdbx persy roughdb)
TX_STRESS_ENGINES=(redb fjall surrealkv heed sled turbokv rocksdb mdbx persy roughdb)
primary_durability() { case "$1" in paritydb-hash|paritydb-btree) echo relaxed ;; *) echo sync ;; esac; }
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
    dur=$(primary_durability "$engine")
    for pattern in "${CORE_PATTERNS[@]}"; do add_job sustained-core "$engine" "$dur" "$pattern" "$CORE_RECORDS" "$CORE_OPS" "$CORE_WINDOW" "$CORE_VALUE" 100 "$trial"; done
  done
  for engine in "${STRESS_ENGINES[@]}"; do
    dur=$(primary_durability "$engine")
    for pattern in append churn; do add_job sustained-4k-stress "$engine" "$dur" "$pattern" "$STRESS_RECORDS" "$STRESS_OPS" "$STRESS_WINDOW" "$STRESS_VALUE" 100 "$trial"; done
  done
  for engine in "${RELAXED_ENGINES[@]}"; do
    for pattern in append churn; do add_job sustained-relaxed "$engine" relaxed "$pattern" "$RELAXED_RECORDS" "$RELAXED_OPS" "$RELAXED_WINDOW" "$RELAXED_VALUE" 100 "$trial"; done
  done
  for engine in "${TX_STRESS_ENGINES[@]}"; do
    add_job sustained-txn-extremes "$engine" "$(primary_durability "$engine")" append "$TX1_RECORDS" "$TX1_OPS" "$TX1_WINDOW" "$TX1_VALUE" 1 "$trial"
    add_job sustained-txn-extremes "$engine" "$(primary_durability "$engine")" append "$TX1000_RECORDS" "$TX1000_OPS" "$TX1000_WINDOW" "$TX1000_VALUE" 1000 "$trial"
  done
done
TOTAL=${#JOBS[@]}
EXPECTED=$(kv_sustained_expected_cases "$PROFILE") || exit 2
(( TOTAL == EXPECTED )) || { echo "KV sustained case-count invariant failed: got=$TOTAL expected=$EXPECTED" >&2; exit 2; }
RESUME_ORDER_POLICY=fixed-initial; [[ "${MATRIX_RESUME_SHUFFLE_REMAINING:-0}" == 1 ]] && RESUME_ORDER_POLICY=reshuffle-remaining
SUPPORT_NEW="$RUN_DIR/support.json.new"
cat > "$SUPPORT_NEW" <<JSON
{"lane":"kv-sustained","profile":"$PROFILE","trials":$TRIALS,"case_count":$TOTAL,"build_profile":"$BUILD_PROFILE","benchmark_binary_sha256":"$BIN_SHA","runner_sha256":"$RUNNER_SHA","sustained_policy_sha256":"$POLICY_SHA","performance_common_sha256":"$COMMON_SHA","noise_guard_sha256":"$NOISE_SHA","admission_policy":"pre-io+pre/post-external-v2","resume_order_policy":"$RESUME_ORDER_POLICY","hostname":"$HOST_NAME","machine_id_sha256":"$MACHINE_ID_SHA256","filesystem":"$FILESYSTEM","source":"$SOURCE","core":{"records":$CORE_RECORDS,"ops":$CORE_OPS,"window_ops":$CORE_WINDOW,"value_bytes":$CORE_VALUE},"stress":{"records":$STRESS_RECORDS,"ops":$STRESS_OPS,"window_ops":$STRESS_WINDOW,"value_bytes":$STRESS_VALUE},"relaxed":{"records":$RELAXED_RECORDS,"ops":$RELAXED_OPS,"window_ops":$RELAXED_WINDOW,"value_bytes":$RELAXED_VALUE},"txn1":{"records":$TX1_RECORDS,"ops":$TX1_OPS,"window_ops":$TX1_WINDOW,"value_bytes":$TX1_VALUE},"txn1000":{"records":$TX1000_RECORDS,"ops":$TX1000_OPS,"window_ops":$TX1000_WINDOW,"value_bytes":$TX1000_VALUE},"window_method":"fixed logical-op windows; no recursive database-size scan between windows","threshold_interpretation":"75/50/25% baseline ratios are reporting diagnostics, not pass/fail criteria","baseline_method":"median throughput and p99 latency of the first min(3, window_count) windows","primary_durability":"sync where durable-before-return is exposed; ParityDB uses its documented relaxed/background path","targeted_sweeps":["4KiB LSM-style compaction stress","relaxed/background durability","txn-size extremes 1 and 1000"],"post_workload_settle_ms":$SETTLE_MS}
JSON
EXISTING_CASES=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
if [[ -s "$RUN_DIR/support.json" ]]; then
  if ! cmp -s "$RUN_DIR/support.json" "$SUPPORT_NEW"; then
    if (( EXISTING_CASES > 0 )); then rm -f "$SUPPORT_NEW"; echo "refusing KV sustained resume: support identity changed" >&2; exit 2; fi
    mv "$SUPPORT_NEW" "$RUN_DIR/support.json"
  else rm -f "$SUPPORT_NEW"; fi
else mv "$SUPPORT_NEW" "$RUN_DIR/support.json"; fi
performance_prepare_order "$RUN_DIR" "$RESUME_ORDER_POLICY" "${JOBS[@]}" || exit $?
if [[ "${MATRIX_PLAN_ONLY:-0}" == 1 ]]; then
  echo "plan-only run=$RUN_ID total=$TOTAL jobs=$RUN_DIR/jobs.txt support=$RUN_DIR/support.json"
  exit 0
fi

INDEX=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1)); IFS='|' read -r scenario engine dur pattern records ops window value txn trial <<< "$job"
  case_id="t${trial}-${scenario}-${engine}-${dur}-${pattern}-n${records}-o${ops}-w${window}-v${value}-tx${txn}"
  out="$RUN_DIR/cases/$case_id.json"; err="$RUN_DIR/stderr/$case_id.log"
  noise_before="$RUN_DIR/noise/$case_id.before.json"; noise_after="$RUN_DIR/noise/$case_id.after.json"
  if [[ -s "$out" ]]; then clear_failure "$case_id"; continue; fi
  echo "[$INDEX/$TOTAL] $case_id" >&2
  performance_check_io_quiet "$PROFILE" "before:$case_id" || exit $?
  performance_check_external_noise "$ROOT" "$PROFILE" "before:$case_id" "$noise_before" || exit $?
  rm -f "$out"
  "$BIN" --engine "$engine" --durability "$dur" --pattern "$pattern" --records "$records" --ops "$ops" --window-ops "$window" --value-bytes "$value" --value-pattern pseudo-random --key-bytes 8 --key-shape sequential --txn-size "$txn" --trial "$trial" --seed 1592606758 --warmup-reads 5000 --settle-ms "$SETTLE_MS" --settle-sample-ms "$SETTLE_SAMPLE_MS" --scenario "$scenario" --root "$DATA_DIR" --output "$out" 2>"$err"
  rc=$?
  noise_rc=0; performance_check_external_noise "$ROOT" "$PROFILE" "after:$case_id" "$noise_after" || noise_rc=$?
  if (( noise_rc != 0 )); then
    rm -f "$out"; clear_failure "$case_id"
    exit "$noise_rc"
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
