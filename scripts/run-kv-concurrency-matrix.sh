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
CASE_MIN_FREE_GIB=${PERFORMANCE_MIN_FREE_GIB:-$MIN_FREE_GIB}

ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
# shellcheck source=kv-matrix-policy.sh
source "$ROOT/scripts/kv-matrix-policy.sh"
# shellcheck source=concurrency-matrix-policy.sh
source "$ROOT/scripts/concurrency-matrix-policy.sh"
# shellcheck source=concurrency-runner-common.sh
source "$ROOT/scripts/concurrency-runner-common.sh"
DEFAULT_CASE_TIMEOUT_S=$(concurrency_case_timeout_s "$PROFILE") || exit 2
PERSY_LOCK_TIMEOUT_MS=${PERSY_LOCK_TIMEOUT_MS:-250}
[[ "$PERSY_LOCK_TIMEOUT_MS" =~ ^[1-9][0-9]*$ ]] || { echo "invalid PERSY_LOCK_TIMEOUT_MS=$PERSY_LOCK_TIMEOUT_MS" >&2; exit 2; }
command -v timeout >/dev/null || { echo "GNU timeout is required" >&2; exit 2; }

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-kv-concurrency-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"; DATA_DIR="${DBBENCH_STORAGE_ROOT:-$ROOT}/data/runs/$RUN_ID"
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
NOISE_SHA=$(sha256sum "$ROOT/tools/benchctl/src/noise.rs" | awk '{print $1}')
CONTINUOUS_NOISE_SHA=$(sha256sum "$ROOT/tools/benchctl/src/noise.rs" | awk '{print $1}')
CONCURRENCY_RUNNER_COMMON_SHA=$(sha256sum "$ROOT/scripts/concurrency-runner-common.sh" | awk '{print $1}')
CONTINUOUS_NOISE_COMMON_SHA=$(sha256sum "$ROOT/scripts/continuous-noise-runner-common.sh" | awk '{print $1}')
HOST_NAME=$(hostname); MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FILESYSTEM=$(findmnt -n -o FSTYPE --target "$DATA_DIR"); SOURCE=$(findmnt -n -o SOURCE --target "$DATA_DIR")
IMPORT_MANIFEST="$RUN_DIR/import-manifest.json"
IMPORT_MANIFEST_SHA=""
if [[ -s "$IMPORT_MANIFEST" ]]; then
  IMPORT_MANIFEST_SHA=$(sha256sum "$IMPORT_MANIFEST" | awk '{print $1}')
  "$ROOT/scripts/benchctl.sh" import-concurrency-results --verify-run "$RUN_DIR" >/dev/null || exit 2
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
SLED_CASE_TIMEOUT_S=$(concurrency_case_timeout_s "$PROFILE" sled) || exit 2
printf '%s' "$(jq -cnS \
  --arg lane kv-concurrency --arg profile "$PROFILE" --argjson trials "$TRIALS" --argjson records "$RECORDS" --argjson default_ops "$OPS" --argjson case_count "$TOTAL" \
  --arg engines "${ENGINES[*]}" --arg clients "${CLIENTS[*]}" --arg core_workloads "${CORE_WORKLOADS[*]}" --arg range_clients "${RANGE_CLIENTS[*]}" --arg delete_clients "${DELETE_CLIENTS[*]}" --arg relaxed_clients "${RELAXED_CLIENTS[*]}" \
  --arg resume "$RESUME_ORDER_POLICY" --arg build "$BUILD_PROFILE" --arg bin_sha "$BIN_SHA" --arg runner_sha "$RUNNER_SHA" --arg common_sha "$CONCURRENCY_RUNNER_COMMON_SHA" --arg noise_common_sha "$CONTINUOUS_NOISE_COMMON_SHA" --arg kv_policy_sha "$KV_POLICY_SHA" --arg policy_sha "$CONCURRENCY_POLICY_SHA" --arg noise_sha "$NOISE_SHA" --arg continuous_sha "$CONTINUOUS_NOISE_SHA" --arg admission "$CONCURRENCY_ADMISSION_POLICY" \
  --argjson sample_ms "$CONTINUOUS_NOISE_SAMPLE_MS" --argjson max_cpu "$CONTINUOUS_NOISE_MAX_CPU_PERCENT" --argjson max_io_avg "$CONTINUOUS_NOISE_MAX_IO_AVERAGE_MIB_S" --argjson max_io_rate "$CONTINUOUS_NOISE_MAX_IO_RATE_MIB_S" --argjson min_free "$MIN_FREE_GIB" --arg case_min_free "$CASE_MIN_FREE_GIB" --argjson timeout "$DEFAULT_CASE_TIMEOUT_S" --argjson sled_timeout "$SLED_CASE_TIMEOUT_S" --argjson persy_timeout "$PERSY_LOCK_TIMEOUT_MS" \
  --arg import_manifest "$IMPORT_MANIFEST_SHA" --arg hostname "$HOST_NAME" --arg machine "$MACHINE_ID_SHA256" --arg filesystem "$FILESYSTEM" --arg source "$SOURCE" \
  '{lane:$lane,profile:$profile,trials:$trials,records:$records,default_ops:$default_ops,case_count:$case_count,shared_database:true,total_ops_fixed_across_client_counts:true,engines:$engines,clients:$clients,core_workloads:$core_workloads,range_clients:$range_clients,delete_clients:$delete_clients,relaxed_clients:$relaxed_clients,resume_order_policy:$resume,build_profile:$build,benchmark_binary_sha256:$bin_sha,runner_sha256:$runner_sha,concurrency_runner_common_sha256:$common_sha,continuous_noise_common_sha256:$noise_common_sha,kv_matrix_policy_sha256:$kv_policy_sha,concurrency_policy_sha256:$policy_sha,noise_guard_sha256:$noise_sha,continuous_noise_guard_sha256:$continuous_sha,admission_policy:$admission,continuous_noise_sample_ms:$sample_ms,continuous_noise_max_cpu_percent:$max_cpu,continuous_noise_max_io_average_mib_s:$max_io_avg,continuous_noise_max_io_rate_mib_s:$max_io_rate,initial_min_free_gib:$min_free,case_min_free_gib:$case_min_free,case_timeout_s:$timeout,engine_case_timeouts_s:{sled:$sled_timeout},persy_lock_timeout_ms:$persy_timeout,state_evolution:"growth",write_pattern:"append",persy_timeout_retry_policy:"typed PrepareError::TransactionTimeout only; max 100 retries; deterministic micro-backoff; retry cost is timed",import_manifest_sha256:($import_manifest|if length==0 then null else . end),hostname:$hostname,machine_id_sha256:$machine,filesystem:$filesystem,source:$source,unsupported:[{engine:"lkv",scope:"all concurrency workloads",reason:"no native clone/shared writer handle"},{engine:"paritydb-hash",scope:"range-scan",reason:"hash-column mode is unordered"}]}')" > "$SUPPORT_NEW"
EXISTING_CASES=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
if [[ -s "$RUN_DIR/support.json" ]]; then
  if ! jq -e -s '.[0] == .[1]' "$RUN_DIR/support.json" "$SUPPORT_NEW" >/dev/null; then
    if (( EXISTING_CASES > 0 )); then rm -f "$SUPPORT_NEW"; echo "refusing KV concurrency resume: support identity changed" >&2; exit 2; fi
    mv "$SUPPORT_NEW" "$RUN_DIR/support.json"
  else rm -f "$SUPPORT_NEW"; fi
else mv "$SUPPORT_NEW" "$RUN_DIR/support.json"; fi
concurrency_prepare_order "$RUN_DIR" "$RESUME_ORDER_POLICY" "${JOBS[@]}" || exit $?
if [[ "${MATRIX_PLAN_ONLY:-0}" == 1 ]]; then
  printf 'run=%s total=%s plan_only=1 support=%s\n' "$RUN_ID" "$TOTAL" "$RUN_DIR/support.json"
  exit 0
fi

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
  noise_before="$RUN_DIR/noise/$case_id.before.json"; noise_during="$RUN_DIR/noise/$case_id.during.json"; noise_after="$RUN_DIR/noise/$case_id.after.json"
  if [[ -s "$out" ]]; then clear_failure "$case_id"; continue; fi
  echo "[$INDEX/$TOTAL] $case_id" >&2
  case_timeout_s=$(concurrency_case_timeout_s "$PROFILE" "$engine") || exit 2
  concurrency_check_io_quiet "$PROFILE" "before:$case_id" || exit $?
  concurrency_check_external_noise "$ROOT" "$PROFILE" "before:$case_id" "$noise_before" || exit $?
  rm -f "$out"
  if [[ "$engine" == persy ]]; then
    DBBENCH_PERSY_LOCK_TIMEOUT_MS="$PERSY_LOCK_TIMEOUT_MS" concurrency_run_with_continuous_noise "$ROOT" "$noise_during" \
      timeout --signal=TERM --kill-after=5s "${case_timeout_s}s" "$BIN" --engine "$engine" --durability "$durability" --workload "$workload" \
      --records "$RECORDS" --ops "$ops" --clients "$clients" --value-bytes 256 --value-pattern pseudo-random \
      --key-bytes 8 --key-shape sequential --access-pattern auto --write-pattern append --state-evolution growth --bounded-churn-slots 0 --txn-size 100 --scan-len "$scan" --warmup-reads 5000 \
      --trial "$trial" --seed 1592606758 --scenario "concurrency-$scenario" --root "$DATA_DIR" --output "$out" 2>"$err"
  else
    concurrency_run_with_continuous_noise "$ROOT" "$noise_during" \
      timeout --signal=TERM --kill-after=5s "${case_timeout_s}s" "$BIN" --engine "$engine" --durability "$durability" --workload "$workload" \
      --records "$RECORDS" --ops "$ops" --clients "$clients" --value-bytes 256 --value-pattern pseudo-random \
      --key-bytes 8 --key-shape sequential --access-pattern auto --write-pattern append --state-evolution growth --bounded-churn-slots 0 --txn-size 100 --scan-len "$scan" --warmup-reads 5000 \
      --trial "$trial" --seed 1592606758 --scenario "concurrency-$scenario" --root "$DATA_DIR" --output "$out" 2>"$err"
  fi
  rc=$?
  noise_rc=0; concurrency_check_external_noise "$ROOT" "$PROFILE" "after:$case_id" "$noise_after" || noise_rc=$?
  if (( noise_rc != 0 )); then
    concurrency_preserve_noise_rejection "$RUN_DIR" "$case_id" post-external "$noise_before" "$noise_during" "$noise_after"
    rm -f "$out"; clear_failure "$case_id"
    exit "$noise_rc"
  fi
  if (( rc == 75 )) && concurrency_continuous_rejected "$noise_during"; then
    concurrency_preserve_noise_rejection "$RUN_DIR" "$case_id" continuous "$noise_before" "$noise_during" "$noise_after"
    rm -f "$out"; clear_failure "$case_id"
    exit 75
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
if "$ROOT/scripts/benchctl.sh" summarize "$RUN_DIR/results.ndjson" --expect-trials "$TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"; then summary_rc=0; else summary_rc=$?; fi
if [[ -s "$RUN_DIR/failures.ndjson" ]]; then FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson"); else FAILURES=0; fi
COMPLETED=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
printf 'run=%s total=%s completed=%s failures=%s summary_rc=%s results=%s\n' "$RUN_ID" "$TOTAL" "$COMPLETED" "$FAILURES" "$summary_rc" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 || summary_rc != 0 || COMPLETED != TOTAL ? 1 : 0 ))
