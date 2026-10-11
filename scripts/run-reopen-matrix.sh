#!/usr/bin/env bash
set -u -o pipefail
PROFILE=${1:-quick}
CACHE=${2:-warm}
case "$PROFILE" in
  smoke) TRIALS=1; RECORDS=1000; OPS=200; MIN_FREE_GIB=2 ;;
  quick) TRIALS=3; RECORDS=100000; OPS=5000; MIN_FREE_GIB=10 ;;
  full) TRIALS=7; RECORDS=1000000; OPS=25000; MIN_FREE_GIB=30 ;;
  *) echo "usage: $0 [smoke|quick|full] [warm|cold]" >&2; exit 2 ;;
esac
CASE_MIN_FREE_GIB=${PERFORMANCE_MIN_FREE_GIB:-$MIN_FREE_GIB}
case "$CACHE" in
  warm) ;;
  cold)
    if (( EUID != 0 )); then
      echo "cold-cache lane requires root for: sync; echo 3 > /proc/sys/vm/drop_caches" >&2
      echo "Run this script as root on an otherwise quiet host; it deliberately does not fake cache eviction." >&2
      exit 77
    fi
    ;;
  *) echo "cache mode must be warm or cold" >&2; exit 2 ;;
esac

ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
# shellcheck source=kv-matrix-policy.sh
source "$ROOT/scripts/kv-matrix-policy.sh"
# shellcheck source=performance-runner-common.sh
source "$ROOT/scripts/performance-runner-common.sh"
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-reopen-$CACHE-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"; DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,noise} "$DATA_DIR"
if [[ "${MATRIX_PLAN_ONLY:-0}" != 1 ]]; then
  free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
  min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
  (( free_bytes >= min_free_bytes )) || { echo "refusing reopen run: free=$free_bytes required=$min_free_bytes" >&2; exit 75; }
  [[ -s "$RUN_DIR/host-start.txt" ]] || "$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
fi

TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"; BUILD_PROFILE=external
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
else
  BIN="$TARGET_DIR/release/kvbench"; BUILD_PROFILE=release
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features kv-all --bin kvbench || exit $?
fi
if [[ -n "${BENCH_BIN_SHA256:-}" ]]; then BIN_SHA="$BENCH_BIN_SHA256"; else BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}'); fi
RUNNER_SHA=$(sha256sum "$ROOT/scripts/run-reopen-matrix.sh" | awk '{print $1}')
KV_POLICY_SHA=$(sha256sum "$ROOT/scripts/kv-matrix-policy.sh" | awk '{print $1}')
COMMON_SHA=$(sha256sum "$ROOT/scripts/performance-runner-common.sh" | awk '{print $1}')
NOISE_SHA=$(sha256sum "$ROOT/scripts/check-external-noise.py" | awk '{print $1}')
CONTINUOUS_NOISE_SHA=$(sha256sum "$ROOT/scripts/run-with-continuous-noise.py" | awk '{print $1}')
HOST_NAME=$(hostname); MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FILESYSTEM=$(findmnt -n -o FSTYPE --target "$DATA_DIR"); SOURCE=$(findmnt -n -o SOURCE --target "$DATA_DIR")

ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
DURS=(relaxed sync)
WORKLOADS=(point-read range-scan)
valid() {
  local engine=$1 dur=$2 workload=$3
  [[ "$engine" == lkv && "$dur" == relaxed ]] && return 1
  [[ "$engine" == manifold && "$dur" == relaxed ]] && return 1
  [[ "$engine" == jammdb && "$dur" == relaxed ]] && return 1
  [[ "$engine" == lsmdb && "$dur" == relaxed ]] && return 1
  [[ "$dur" == sync && ( "$engine" == paritydb-hash || "$engine" == paritydb-btree ) ]] && return 1
  [[ "$engine" == lkv && "$workload" == range-scan ]] && return 1
  [[ "$engine" == paritydb-hash && "$workload" == range-scan ]] && return 1
  return 0
}
JOBS=()
for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    for dur in "${DURS[@]}"; do
      for workload in "${WORKLOADS[@]}"; do
        valid "$engine" "$dur" "$workload" || continue
        case_ops=$(kv_effective_ops "$engine" "$workload" "$OPS" "$RECORDS") || exit 2
        JOBS+=("$engine|$dur|$workload|$case_ops|$trial")
      done
    done
  done
done
TOTAL=${#JOBS[@]}
EXPECTED=$((50 * TRIALS))
(( TOTAL == EXPECTED )) || { echo "reopen case-count invariant failed: got=$TOTAL expected=$EXPECTED" >&2; exit 2; }
RESUME_ORDER_POLICY=fixed-initial; [[ "${MATRIX_RESUME_SHUFFLE_REMAINING:-0}" == 1 ]] && RESUME_ORDER_POLICY=reshuffle-remaining
SUPPORT_NEW="$RUN_DIR/support.json.new"
cat > "$SUPPORT_NEW" <<JSON
{"lane":"kv-reopen","profile":"$PROFILE","cache_mode":"$CACHE","trials":$TRIALS,"records":$RECORDS,"default_ops":$OPS,"case_count":$TOTAL,"build_profile":"$BUILD_PROFILE","benchmark_binary_sha256":"$BIN_SHA","runner_sha256":"$RUNNER_SHA","kv_matrix_policy_sha256":"$KV_POLICY_SHA","performance_common_sha256":"$COMMON_SHA","noise_guard_sha256":"$NOISE_SHA","continuous_noise_guard_sha256":"$CONTINUOUS_NOISE_SHA","admission_policy":"$PERFORMANCE_ADMISSION_POLICY","continuous_noise_sample_ms":$CONTINUOUS_NOISE_SAMPLE_MS,"continuous_noise_max_cpu_percent":$CONTINUOUS_NOISE_MAX_CPU_PERCENT,"continuous_noise_max_io_average_mib_s":$CONTINUOUS_NOISE_MAX_IO_AVERAGE_MIB_S,"continuous_noise_max_io_rate_mib_s":$CONTINUOUS_NOISE_MAX_IO_RATE_MIB_S,"initial_min_free_gib":$MIN_FREE_GIB,"case_min_free_gib":"$CASE_MIN_FREE_GIB","resume_order_policy":"$RESUME_ORDER_POLICY","hostname":"$HOST_NAME","machine_id_sha256":"$MACHINE_ID_SHA256","filesystem":"$FILESYSTEM","source":"$SOURCE","open_pressure_evidence":"format v7 open_process + open_system_delta retained as diagnostic evidence; never used for acceptance","prepare_semantics":"fresh prefill with one discarded read; measured process reopens same DB with prefill and warmup skipped","cold_cache_semantics":"root-only global sync + drop_caches; never simulated"}
JSON
EXISTING_CASES=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
if [[ -s "$RUN_DIR/support.json" ]]; then
  if ! cmp -s "$RUN_DIR/support.json" "$SUPPORT_NEW"; then
    if (( EXISTING_CASES > 0 )); then rm -f "$SUPPORT_NEW"; echo "refusing reopen resume: support identity changed" >&2; exit 2; fi
    mv "$SUPPORT_NEW" "$RUN_DIR/support.json"
  else rm -f "$SUPPORT_NEW"; fi
else mv "$SUPPORT_NEW" "$RUN_DIR/support.json"; fi
performance_prepare_order "$RUN_DIR" "$RESUME_ORDER_POLICY" "${JOBS[@]}" || exit $?
if [[ "${MATRIX_PLAN_ONLY:-0}" == 1 ]]; then
  echo "plan-only run=$RUN_ID total=$TOTAL jobs=$RUN_DIR/jobs.txt support=$RUN_DIR/support.json"
  exit 0
fi
clear_failure() {
  local case_id=$1 failures="$RUN_DIR/failures.ndjson" tmp
  [[ -f "$failures" ]] || return 0
  tmp="${failures}.tmp"; jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' "$failures" > "$tmp"; mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}

INDEX=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1)); IFS='|' read -r engine dur workload case_ops trial <<< "$job"
  db_name="reopen-${trial}-${engine}-${dur}-${workload}"
  case_id="t${trial}-reopen-${CACHE}-${engine}-${dur}-${workload}-n${RECORDS}-o${case_ops}"
  out="$RUN_DIR/cases/$case_id.json"; err="$RUN_DIR/stderr/$case_id.log"
  noise_before="$RUN_DIR/noise/$case_id.before.json"; noise_ready="$RUN_DIR/noise/$case_id.ready.json"; noise_during="$RUN_DIR/noise/$case_id.during.json"; noise_after="$RUN_DIR/noise/$case_id.after.json"
  if [[ -s "$out" ]]; then clear_failure "$case_id"; continue; fi
  echo "[$INDEX/$TOTAL] $case_id" >&2
  performance_check_io_quiet "$PROFILE" "before-prepare:$case_id" || exit $?
  performance_check_external_noise "$ROOT" "$PROFILE" "before-prepare:$case_id" "$noise_before" || exit $?

  "$BIN" --engine "$engine" --durability "$dur" --workload "$workload" \
    --records "$RECORDS" --ops 1 --value-bytes 256 --txn-size 100 --scan-len 100 \
    --trial "$trial" --seed 1592606758 --scenario reopen-prepare --db-name "$db_name" \
    --root "$DATA_DIR" --output /dev/null --keep-db --warmup-reads 0 \
    > /dev/null 2>"$RUN_DIR/stderr/$case_id.prepare.log"
  prepare_rc=$?
  if (( prepare_rc != 0 )); then
    clear_failure "$case_id"
    jq -cn --arg case_id "$case_id" --arg stderr "$RUN_DIR/stderr/$case_id.prepare.log" --argjson rc "$prepare_rc" '{case_id:$case_id,phase:"prepare",returncode:$rc,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
    continue
  fi

  if [[ "$CACHE" == cold ]]; then sync; echo 3 > /proc/sys/vm/drop_caches; fi
  performance_check_io_quiet "$PROFILE" "before-reopen:$case_id" || exit $?
  performance_check_external_noise "$ROOT" "$PROFILE" "before-reopen:$case_id" "$noise_ready" || exit $?

  performance_run_with_continuous_noise "$ROOT" "$noise_during" "$BIN" --engine "$engine" --durability "$dur" --workload "$workload" \
    --records "$RECORDS" --ops "$case_ops" --value-bytes 256 --txn-size 100 --scan-len 100 \
    --trial "$trial" --seed 1592606758 --scenario "reopen-$CACHE" --db-name "$db_name" \
    --root "$DATA_DIR" --output "$out" --reuse-db --skip-prefill --warmup-reads 0 2>"$err"
  rc=$?
  noise_rc=0; performance_check_external_noise "$ROOT" "$PROFILE" "after:$case_id" "$noise_after" || noise_rc=$?
  if (( noise_rc != 0 )); then
    performance_preserve_noise_rejection "$RUN_DIR" "$case_id" post-external "$noise_ready" "$noise_during" "$noise_after"
    rm -f "$out"; clear_failure "$case_id"
    exit "$noise_rc"
  fi
  if (( rc == 75 )) && performance_continuous_rejected "$noise_during"; then
    performance_preserve_noise_rejection "$RUN_DIR" "$case_id" continuous "$noise_ready" "$noise_during" "$noise_after"
    rm -f "$out"; clear_failure "$case_id"
    exit 75
  fi
  if (( rc != 0 )) || [[ ! -s "$out" ]] || [[ $(wc -l < "$out") -ne 1 ]]; then
    rm -f "$out"; clear_failure "$case_id"
    jq -cn --arg case_id "$case_id" --arg stderr "$err" --argjson rc "$rc" '{case_id:$case_id,phase:"reopen",returncode:$rc,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
    continue
  fi
  clear_failure "$case_id"; [[ -s "$err" ]] || rm -f "$err"
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
if uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" --expect-trials "$TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"; then summary_rc=0; else summary_rc=$?; fi
if [[ -s "$RUN_DIR/failures.ndjson" ]]; then FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson"); else FAILURES=0; fi
COMPLETED=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
printf 'run=%s cache=%s total=%s completed=%s failures=%s summary_rc=%s results=%s\n' "$RUN_ID" "$CACHE" "$TOTAL" "$COMPLETED" "$FAILURES" "$summary_rc" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 || summary_rc != 0 || COMPLETED != TOTAL ? 1 : 0 ))
