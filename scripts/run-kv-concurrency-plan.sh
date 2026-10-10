#!/usr/bin/env bash
set -u -o pipefail

PLAN=${1:?usage: run-kv-concurrency-plan.sh PLAN.json [RUN_ID]}
RUN_ID=${2:-"$(date -u +%Y%m%dT%H%M%SZ)-kv-concurrency-probe"}
ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
PROFILE=quick
MIN_FREE_GIB=10
CASE_MIN_FREE_GIB=${PERFORMANCE_MIN_FREE_GIB:-$MIN_FREE_GIB}
# shellcheck source=concurrency-matrix-policy.sh
source "$ROOT/scripts/concurrency-matrix-policy.sh"
# shellcheck source=concurrency-runner-common.sh
source "$ROOT/scripts/concurrency-runner-common.sh"
CASE_TIMEOUT_S=$(concurrency_case_timeout_s "$PROFILE") || exit 2
PERSY_LOCK_TIMEOUT_MS=${PERSY_LOCK_TIMEOUT_MS:-250}
[[ "$PERSY_LOCK_TIMEOUT_MS" =~ ^[1-9][0-9]*$ ]] || { echo "invalid PERSY_LOCK_TIMEOUT_MS=$PERSY_LOCK_TIMEOUT_MS" >&2; exit 2; }
command -v timeout >/dev/null || { echo "GNU timeout is required" >&2; exit 2; }
command -v ionice >/dev/null || { echo "ionice is required for low-priority database preparation" >&2; exit 2; }
[[ -s "$PLAN" ]] || { echo "plan not found: $PLAN" >&2; exit 2; }
PLAN=$(readlink -f "$PLAN")
PLAN_SHA=$(sha256sum "$PLAN" | awk '{print $1}')

RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,noise} "$DATA_DIR"
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
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
  if [[ "${BENCH_BIN_PREVERIFIED:-0}" != 1 ]]; then
    actual=$(sha256sum "$BIN" | awk '{print $1}')
    [[ "$actual" == "$BIN_SHA" ]] || { echo "BENCH_BIN SHA mismatch: $actual != $BIN_SHA" >&2; exit 2; }
  fi
else
  BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}')
fi

RUNNER_SHA=$(sha256sum "$ROOT/scripts/run-kv-concurrency-plan.sh" | awk '{print $1}')
NOISE_SHA=$(sha256sum "$ROOT/scripts/check-external-noise.py" | awk '{print $1}')
CONCURRENCY_POLICY_SHA=$(sha256sum "$ROOT/scripts/concurrency-matrix-policy.sh" | awk '{print $1}')
HOST_NAME=$(hostname); MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FILESYSTEM=$(findmnt -n -o FSTYPE --target "$DATA_DIR"); SOURCE=$(findmnt -n -o SOURCE --target "$DATA_DIR")
HARNESS_COMMIT=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)
BENCH_SOURCE_COMMIT=${BENCH_SOURCE_COMMIT:-$HARNESS_COMMIT}

PLAN_TSV="$RUN_DIR/plan.tsv.new"
uv run --script "$ROOT/scripts/validate-concurrency-plan.py" "$PLAN" "$PLAN_TSV" "$RUN_DIR/plan-meta.json.new" || exit $?

if [[ -s "$RUN_DIR/plan.tsv" ]]; then
  if ! cmp -s "$RUN_DIR/plan.tsv" "$PLAN_TSV"; then
    rm -f "$PLAN_TSV"; echo "refusing plan resume: materialized plan changed" >&2; exit 2
  fi
  rm -f "$PLAN_TSV"
else
  mv "$PLAN_TSV" "$RUN_DIR/plan.tsv"
fi
if [[ -s "$RUN_DIR/plan-meta.json" ]]; then
  if ! cmp -s "$RUN_DIR/plan-meta.json" "$RUN_DIR/plan-meta.json.new"; then
    rm -f "$RUN_DIR/plan-meta.json.new"; echo "refusing plan resume: plan metadata changed" >&2; exit 2
  fi
  rm -f "$RUN_DIR/plan-meta.json.new"
else
  mv "$RUN_DIR/plan-meta.json.new" "$RUN_DIR/plan-meta.json"
fi
PLAN_VERSION=$(jq -r .plan_version "$RUN_DIR/plan-meta.json")
EXPECT_TRIALS=$(jq -r .expect_trials "$RUN_DIR/plan-meta.json")
PLAN_KIND=$(jq -r '.kind // "stock-client-group-coverage"' "$PLAN")
if (( PLAN_VERSION >= 2 )); then
  SUPPORT_LANE=kv-concurrency
  PREPARED_DB_PROTOCOL=case-private-clean-close-v1
else
  SUPPORT_LANE=kv-concurrency-probe
  PREPARED_DB_PROTOCOL=legacy-in-process
fi
mapfile -t JOBS < "$RUN_DIR/plan.tsv"
TOTAL=${#JOBS[@]}
(( TOTAL > 0 )) || { echo "empty plan" >&2; exit 2; }
(( TOTAL % EXPECT_TRIALS == 0 )) || { echo "plan case count is not divisible by expect_trials" >&2; exit 2; }

SUPPORT_NEW="$RUN_DIR/support.json.new"
python3 - "$SUPPORT_NEW" <<PY_SUPPORT
import json
json.dump({
 "lane":"$SUPPORT_LANE","profile":"quick","case_count":$TOTAL,"trials":$EXPECT_TRIALS,"expect_trials":$EXPECT_TRIALS,"plan_version":$PLAN_VERSION,"plan_kind":"$PLAN_KIND",
 "clients":"1 2 4 8","range_clients":"1 4 8","delete_clients":"1 4 8","relaxed_clients":"1 4 8",
 "plan_path":"$PLAN","plan_sha256":"$PLAN_SHA","build_profile":"$BUILD_PROFILE","benchmark_binary_sha256":"$BIN_SHA","benchmark_source_commit":"$BENCH_SOURCE_COMMIT","harness_commit":"$HARNESS_COMMIT",
 "runner_sha256":"$RUNNER_SHA","concurrency_policy_sha256":"$CONCURRENCY_POLICY_SHA","noise_guard_sha256":"$NOISE_SHA","admission_policy":"pre-io+pre/post-external-v2","initial_min_free_gib":$MIN_FREE_GIB,"case_min_free_gib":"$CASE_MIN_FREE_GIB",
 "case_timeout_s":$CASE_TIMEOUT_S,"persy_lock_timeout_ms":$PERSY_LOCK_TIMEOUT_MS,
 "prepared_db_protocol":"$PREPARED_DB_PROTOCOL",
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
  IFS='|' read -r scenario engine durability workload clients records ops trial state_evolution write_pattern bounded_churn_slots <<< "$job"
  scenario_tag=${scenario#concurrency-}
  case_id="t${trial}-${scenario_tag}-${engine}-${durability}-${workload}-se${state_evolution}-wp${write_pattern}-bs${bounded_churn_slots}-c${clients}-n${records}-o${ops}"
  out="$RUN_DIR/cases/$case_id.json"; err="$RUN_DIR/stderr/$case_id.log"
  noise_before="$RUN_DIR/noise/$case_id.before.json"; noise_after="$RUN_DIR/noise/$case_id.after.json"
  if [[ -s "$out" ]]; then clear_failure "$case_id"; continue; fi
  echo "[$INDEX/$TOTAL] $case_id" >&2

  if (( PLAN_VERSION >= 2 )); then
    case_root="$DATA_DIR/$case_id"
    prepared_marker="$case_root/.dbbench-prepared-v1.json"
    marker_ok=0
    if [[ -s "$prepared_marker" ]] && jq -e \
      --arg case_id "$case_id" \
      --arg plan_sha256 "$PLAN_SHA" \
      --arg binary_sha256 "$BIN_SHA" \
      --arg harness_commit "$HARNESS_COMMIT" \
      --arg runner_sha256 "$RUNNER_SHA" \
      '.case_id == $case_id and .plan_sha256 == $plan_sha256 and .binary_sha256 == $binary_sha256 and .harness_commit == $harness_commit and .runner_sha256 == $runner_sha256' \
      "$prepared_marker" >/dev/null 2>&1; then
      marker_ok=1
    fi
    if (( marker_ok == 0 )); then
      rm -rf "$case_root" || exit 2
      mkdir -p "$case_root" || exit 2
      rm -f "$out"
      prep_cmd=("$BIN" --engine "$engine" --durability "$durability" --workload "$workload" --records "$records" --ops "$ops" --clients "$clients" --value-bytes 256 --value-pattern pseudo-random --key-bytes 8 --key-shape sequential --access-pattern auto --write-pattern "$write_pattern" --state-evolution "$state_evolution" --bounded-churn-slots "$bounded_churn_slots" --txn-size 100 --scan-len 100 --warmup-reads 5000 --trial "$trial" --seed 1592606758 --scenario "$scenario" --root "$case_root" --output "$out" --prepare-only)
      if [[ "$engine" == persy ]]; then
        DBBENCH_PERSY_LOCK_TIMEOUT_MS="$PERSY_LOCK_TIMEOUT_MS" ionice -c2 -n7 nice -n15 timeout --signal=TERM --kill-after=5s "${CASE_TIMEOUT_S}s" "${prep_cmd[@]}" 2>"$err"
      else
        ionice -c2 -n7 nice -n15 timeout --signal=TERM --kill-after=5s "${CASE_TIMEOUT_S}s" "${prep_cmd[@]}" 2>"$err"
      fi
      prep_rc=$?
      if (( prep_rc != 0 )); then
        rm -rf "$case_root"; rm -f "$out"; clear_failure "$case_id"
        failure_kind=prepare-error; (( prep_rc == 124 || prep_rc == 137 )) && failure_kind=prepare-timeout
        jq -cn --arg case_id "$case_id" --arg stderr "$err" --arg failure_kind "$failure_kind" --argjson rc "$prep_rc" '{case_id:$case_id,returncode:$rc,failure_kind:$failure_kind,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
        continue
      fi
      marker_tmp="$prepared_marker.tmp.$$"
      if ! jq -cn --arg case_id "$case_id" --arg plan_sha256 "$PLAN_SHA" --arg binary_sha256 "$BIN_SHA" --arg harness_commit "$HARNESS_COMMIT" --arg runner_sha256 "$RUNNER_SHA" \
        '{version:1,case_id:$case_id,plan_sha256:$plan_sha256,binary_sha256:$binary_sha256,harness_commit:$harness_commit,runner_sha256:$runner_sha256}' > "$marker_tmp"; then
        rm -rf "$case_root"; exit 2
      fi
      if ! mv "$marker_tmp" "$prepared_marker"; then
        rm -rf "$case_root"; exit 2
      fi
      [[ -s "$err" ]] || rm -f "$err"
    fi

    # Preparation is intentionally outside the performance admission window. It is
    # low CPU/I/O priority and cleanly closed. A fresh gate below admits only reopen,
    # warmup and the measured client interval.
    concurrency_check_io_quiet "$PROFILE" "before:$case_id" || exit $?
    concurrency_check_external_noise "$ROOT" "$PROFILE" "before:$case_id" "$noise_before" || exit $?
    rm -f "$out"
    cmd=("$BIN" --engine "$engine" --durability "$durability" --workload "$workload" --records "$records" --ops "$ops" --clients "$clients" --value-bytes 256 --value-pattern pseudo-random --key-bytes 8 --key-shape sequential --access-pattern auto --write-pattern "$write_pattern" --state-evolution "$state_evolution" --bounded-churn-slots "$bounded_churn_slots" --txn-size 100 --scan-len 100 --warmup-reads 5000 --trial "$trial" --seed 1592606758 --scenario "$scenario" --root "$case_root" --output "$out" --reuse-db)
  else
    # Historical v1 probe semantics remain unchanged for pinned pre-v6 binaries.
    concurrency_check_io_quiet "$PROFILE" "before:$case_id" || exit $?
    concurrency_check_external_noise "$ROOT" "$PROFILE" "before:$case_id" "$noise_before" || exit $?
    rm -f "$out"
    case_root=""
    cmd=("$BIN" --engine "$engine" --durability "$durability" --workload "$workload" --records "$records" --ops "$ops" --clients "$clients" --value-bytes 256 --value-pattern pseudo-random --key-bytes 8 --key-shape sequential --access-pattern auto --write-pattern "$write_pattern" --txn-size 100 --scan-len 100 --warmup-reads 5000 --trial "$trial" --seed 1592606758 --scenario "$scenario" --root "$DATA_DIR" --output "$out")
  fi

  if [[ "$engine" == persy ]]; then
    DBBENCH_PERSY_LOCK_TIMEOUT_MS="$PERSY_LOCK_TIMEOUT_MS" timeout --signal=TERM --kill-after=5s "${CASE_TIMEOUT_S}s" "${cmd[@]}" 2>"$err"
  else
    timeout --signal=TERM --kill-after=5s "${CASE_TIMEOUT_S}s" "${cmd[@]}" 2>"$err"
  fi
  rc=$?
  noise_rc=0; concurrency_check_external_noise "$ROOT" "$PROFILE" "after:$case_id" "$noise_after" || noise_rc=$?
  if (( noise_rc != 0 )); then
    rm -f "$out"; clear_failure "$case_id"
    (( PLAN_VERSION >= 2 )) && rm -rf "$case_root"
    exit "$noise_rc"
  fi
  if (( rc != 0 )) || [[ ! -s "$out" ]] || [[ $(wc -l < "$out") -ne 1 ]]; then
    rm -f "$out"; clear_failure "$case_id"
    (( PLAN_VERSION >= 2 )) && rm -rf "$case_root"
    failure_kind=benchmark-error; (( rc == 124 || rc == 137 )) && failure_kind=case-timeout
    jq -cn --arg case_id "$case_id" --arg stderr "$err" --arg failure_kind "$failure_kind" --argjson rc "$rc" '{case_id:$case_id,returncode:$rc,failure_kind:$failure_kind,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
    continue
  fi
  (( PLAN_VERSION >= 2 )) && rm -rf "$case_root"
  clear_failure "$case_id"; [[ -s "$err" ]] || rm -f "$err"
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
if uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" --expect-trials "$EXPECT_TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"; then summary_rc=0; else summary_rc=$?; fi
FAILURES=0; [[ -s "$RUN_DIR/failures.ndjson" ]] && FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson")
COMPLETED=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
printf 'run=%s total=%s completed=%s failures=%s summary_rc=%s results=%s\n' "$RUN_ID" "$TOTAL" "$COMPLETED" "$FAILURES" "$summary_rc" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 || summary_rc != 0 || COMPLETED != TOTAL ? 1 : 0 ))
