#!/usr/bin/env bash
set -u -o pipefail

PLAN=${1:?usage: run-record-concurrency-plan.sh PLAN.json [RUN_ID]}
RUN_ID=${2:-"$(date -u +%Y%m%dT%H%M%SZ)-record-concurrency-plan"}
ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
PROFILE=quick
# shellcheck source=concurrency-matrix-policy.sh
source "$ROOT/scripts/concurrency-matrix-policy.sh"
# shellcheck source=concurrency-runner-common.sh
source "$ROOT/scripts/concurrency-runner-common.sh"
CASE_TIMEOUT_S=$(concurrency_case_timeout_s "$PROFILE") || exit 2
command -v timeout >/dev/null || { echo "GNU timeout is required" >&2; exit 2; }
[[ -s "$PLAN" ]] || { echo "plan not found: $PLAN" >&2; exit 2; }
PLAN=$(readlink -f "$PLAN")
PLAN_SHA=$(sha256sum "$PLAN" | awk '{print $1}')

RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,noise} "$DATA_DIR"
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((10 * 1024 * 1024 * 1024))
(( free_bytes >= min_free_bytes )) || { echo "refusing record concurrency plan: free=$free_bytes required=$min_free_bytes" >&2; exit 75; }
"$ROOT/scripts/ensure-sqlite-3.53.4.sh" >/dev/null
[[ -s "$RUN_DIR/host-start.txt" ]] || "$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"

TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"; BUILD_PROFILE=external
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
else
  BIN="$TARGET_DIR/release/recordconcurrency"; BUILD_PROFILE=release
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features record --bin recordconcurrency || exit $?
fi
if [[ -n "${BENCH_BIN_SHA256:-}" ]]; then
  BIN_SHA="$BENCH_BIN_SHA256"
else
  BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}')
fi

ROCKS_TARGET_DIR=${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}
if [[ -n "${ROCKS_BENCH_BIN:-}" ]]; then
  ROCKS_BIN="$ROCKS_BENCH_BIN"; ROCKS_BUILD_PROFILE=external
  [[ -x "$ROCKS_BIN" ]] || { echo "ROCKS_BENCH_BIN is not executable: $ROCKS_BIN" >&2; exit 2; }
else
  ROCKS_BIN="$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordconcurrency"; ROCKS_BUILD_PROFILE=release
  "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked \
    --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" \
    --target-dir "$ROCKS_TARGET_DIR" --bin surrealdb-rocksdb-recordconcurrency || exit $?
fi
if [[ -n "${ROCKS_BENCH_BIN_SHA256:-}" ]]; then
  ROCKS_BIN_SHA="$ROCKS_BENCH_BIN_SHA256"
else
  ROCKS_BIN_SHA=$(sha256sum "$ROCKS_BIN" | awk '{print $1}')
fi

RUNNER_SHA=$(sha256sum "$ROOT/scripts/run-record-concurrency-plan.sh" | awk '{print $1}')
NOISE_SHA=$(sha256sum "$ROOT/scripts/check-external-noise.py" | awk '{print $1}')
PRESSURE_SHA=$(sha256sum "$ROOT/scripts/scrub-short-trial-pressure.py" | awk '{print $1}')
POLICY_SHA=$(sha256sum "$ROOT/scripts/concurrency-matrix-policy.sh" | awk '{print $1}')
HOST_NAME=$(hostname)
MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FILESYSTEM=$(findmnt -n -o FSTYPE --target "$DATA_DIR")
SOURCE=$(findmnt -n -o SOURCE --target "$DATA_DIR")
HARNESS_COMMIT=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)
BENCH_SOURCE_COMMIT=${BENCH_SOURCE_COMMIT:-$HARNESS_COMMIT}
ROCKS_BENCH_SOURCE_COMMIT=${ROCKS_BENCH_SOURCE_COMMIT:-$BENCH_SOURCE_COMMIT}

PLAN_TSV="$RUN_DIR/plan.tsv.new"
META_NEW="$RUN_DIR/plan-meta.json.new"
uv run --script "$ROOT/scripts/validate-record-concurrency-plan.py" "$PLAN" "$PLAN_TSV" "$META_NEW" || exit $?
if [[ -s "$RUN_DIR/plan.tsv" ]]; then
  cmp -s "$RUN_DIR/plan.tsv" "$PLAN_TSV" || { rm -f "$PLAN_TSV" "$META_NEW"; echo "refusing plan resume: materialized plan changed" >&2; exit 2; }
  rm -f "$PLAN_TSV"
else
  mv "$PLAN_TSV" "$RUN_DIR/plan.tsv"
fi
if [[ -s "$RUN_DIR/plan-meta.json" ]]; then
  cmp -s "$RUN_DIR/plan-meta.json" "$META_NEW" || { rm -f "$META_NEW"; echo "refusing plan resume: plan metadata changed" >&2; exit 2; }
  rm -f "$META_NEW"
else
  mv "$META_NEW" "$RUN_DIR/plan-meta.json"
fi

EXPECT_TRIALS=$(jq -r .expect_trials "$RUN_DIR/plan-meta.json")
PLAN_VERSION=$(jq -r .plan_version "$RUN_DIR/plan-meta.json")
PLAN_KIND=$(jq -r .kind "$RUN_DIR/plan-meta.json")
READ_MATERIALIZATION=$(jq -r .read_materialization "$RUN_DIR/plan-meta.json")
WRITE_MATERIALIZATION=$(jq -r .write_materialization "$RUN_DIR/plan-meta.json")
mapfile -t JOBS < "$RUN_DIR/plan.tsv"
TOTAL=${#JOBS[@]}
(( TOTAL > 0 )) || { echo "empty plan" >&2; exit 2; }
(( TOTAL % EXPECT_TRIALS == 0 )) || { echo "plan case count not divisible by trials" >&2; exit 2; }

SUPPORT_NEW="$RUN_DIR/support.json.new"
python3 - "$SUPPORT_NEW" <<PY_SUPPORT
import json
json.dump({
 "lane":"record-concurrency","profile":"quick","case_count":$TOTAL,"trials":$EXPECT_TRIALS,"expect_trials":$EXPECT_TRIALS,
 "plan_version":$PLAN_VERSION,"plan_kind":"$PLAN_KIND","read_materialization":"$READ_MATERIALIZATION","write_materialization":"$WRITE_MATERIALIZATION","plan_path":"$PLAN","plan_sha256":"$PLAN_SHA",
 "engines":"surrealdb turso sqlite surrealdb-rocksdb",
 "clients":"1 2 4 8","relaxed_clients":"1 4 8","stress_clients":"1 4 8","tx_clients":"1 4 8",
 "build_profile":"$BUILD_PROFILE","benchmark_binary_sha256":"$BIN_SHA","benchmark_source_commit":"$BENCH_SOURCE_COMMIT",
 "rocks_build_profile":"$ROCKS_BUILD_PROFILE","surrealdb_rocksdb_binary_sha256":"$ROCKS_BIN_SHA",
 "surrealdb_rocksdb_source_commit":"$ROCKS_BENCH_SOURCE_COMMIT","harness_commit":"$HARNESS_COMMIT",
 "runner_sha256":"$RUNNER_SHA","concurrency_policy_sha256":"$POLICY_SHA","noise_guard_sha256":"$NOISE_SHA",
 "short_pressure_guard_sha256":"$PRESSURE_SHA","case_timeout_s":$CASE_TIMEOUT_S,
 "hostname":"$HOST_NAME","machine_id_sha256":"$MACHINE_ID_SHA256","filesystem":"$FILESYSTEM","source":"$SOURCE",
 "total_work_semantics":"each plan family keeps identical records and total ops across client counts",
 "state_evolution_semantics":"bounded tiny/write updates the prefilled record universe; growth preserves append diagnostics",
 "surreal_conflict_policy":"retry typed transaction conflicts only; retry time stays inside measured latency"
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
  tmp="${failures}.tmp"
  jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' "$failures" > "$tmp"
  mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}

INDEX=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1))
  IFS='|' read -r scenario engine durability workload clients records ops payload txn trial state_evolution read_materialization write_materialization <<< "$job"
  case_id="t${trial}-${scenario}-${engine}-${durability}-${workload}-se${state_evolution}-rm${read_materialization}-wm${write_materialization}-c${clients}-n${records}-o${ops}-p${payload}-tx${txn}"
  out="$RUN_DIR/cases/$case_id.json"
  err="$RUN_DIR/stderr/$case_id.log"
  noise_before="$RUN_DIR/noise/$case_id.before.json"
  noise_after="$RUN_DIR/noise/$case_id.after.json"
  if [[ -s "$out" ]]; then clear_failure "$case_id"; continue; fi

  echo "[$INDEX/$TOTAL] $case_id" >&2
  concurrency_check_io_quiet "$PROFILE" "before:$case_id" || exit $?
  concurrency_check_external_noise "$ROOT" "$PROFILE" "before:$case_id" "$noise_before" || exit $?
  CASE_BIN="$BIN"
  [[ "$engine" == surrealdb-rocksdb ]] && CASE_BIN="$ROCKS_BIN"
  rm -f "$out"
  timeout --signal=TERM --kill-after=5s "${CASE_TIMEOUT_S}s" \
    "$CASE_BIN" --engine "$engine" --durability "$durability" --workload "$workload" \
    --state-evolution "$state_evolution" --clients "$clients" --records "$records" --ops "$ops" \
    --payload-bytes "$payload" --txn-size "$txn" --trial "$trial" --seed 1592606758 \
    --scenario "$scenario" --warmup-reads 5000 --root "$DATA_DIR" --output "$out" 2>"$err"
  rc=$?

  io_rc=0; concurrency_check_io_quiet "$PROFILE" "after:$case_id" || io_rc=$?
  noise_rc=0; concurrency_check_external_noise "$ROOT" "$PROFILE" "after:$case_id" "$noise_after" || noise_rc=$?
  if (( io_rc != 0 || noise_rc != 0 )); then
    rm -f "$out"; clear_failure "$case_id"
    if (( io_rc != 0 )); then exit "$io_rc"; else exit "$noise_rc"; fi
  fi
  if (( rc == 0 )) && [[ -s "$out" ]] && [[ $(wc -l < "$out") -eq 1 ]]; then
    if ! jq -e --arg read_expected "$read_materialization" --arg write_expected "$write_materialization" \
      '.read_materialization == $read_expected and .write_materialization == $write_expected' "$out" >/dev/null; then
      rc=2
      printf 'record result semantic identity mismatch: expected read_materialization=%s write_materialization=%s\n' \
        "$read_materialization" "$write_materialization" >>"$err"
    fi
  fi
  if (( rc != 0 )) || [[ ! -s "$out" ]] || [[ $(wc -l < "$out") -ne 1 ]]; then
    rm -f "$out"; clear_failure "$case_id"
    failure_kind=benchmark-error
    (( rc == 124 || rc == 137 )) && failure_kind=case-timeout
    jq -cn --arg case_id "$case_id" --arg stderr "$err" --arg failure_kind "$failure_kind" --argjson rc "$rc" \
      '{case_id:$case_id,returncode:$rc,failure_kind:$failure_kind,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
    continue
  fi
  pressure_report=$(mktemp)
  pressure_rc=0
  concurrency_scrub_case_pressure "$ROOT" "$PROFILE" "$RUN_DIR" "$case_id" "$pressure_report" || pressure_rc=$?
  rm -f "$pressure_report"
  if (( pressure_rc != 0 )); then clear_failure "$case_id"; exit "$pressure_rc"; fi
  clear_failure "$case_id"
  [[ -s "$err" ]] || rm -f "$err"
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
if uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" --expect-trials "$EXPECT_TRIALS" \
    --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"; then
  summary_rc=0
else
  summary_rc=$?
fi
FAILURES=0
[[ -s "$RUN_DIR/failures.ndjson" ]] && FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson")
COMPLETED=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
printf 'run=%s total=%s completed=%s failures=%s summary_rc=%s results=%s\n' \
  "$RUN_ID" "$TOTAL" "$COMPLETED" "$FAILURES" "$summary_rc" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 || summary_rc != 0 || COMPLETED != TOTAL ? 1 : 0 ))
