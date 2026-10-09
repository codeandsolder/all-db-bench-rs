#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke) RECORDS=100; OPS=100; TRIALS=1; MIN_FREE_GIB=2 ;;
  quick) RECORDS=100000; OPS=50000; TRIALS=3; MIN_FREE_GIB=10 ;;
  full)  RECORDS=1000000; OPS=250000; TRIALS=7; MIN_FREE_GIB=30 ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac
TRIALS=${KV_TRIALS_OVERRIDE:-$TRIALS}
RECORDS=${KV_RECORDS_OVERRIDE:-$RECORDS}
OPS=${KV_OPS_OVERRIDE:-$OPS}

ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
# shellcheck source=kv-matrix-policy.sh
source "$ROOT/scripts/kv-matrix-policy.sh"
KV_LSMDB_RANGE_TARGET_TRAVERSED_ENTRIES=${KV_LSMDB_RANGE_TARGET_TRAVERSED_ENTRIES:-50000000}
KV_LSMDB_RANGE_MIN_OPS=${KV_LSMDB_RANGE_MIN_OPS:-50}
LSMDB_RANGE_OPS=$(kv_effective_ops lsmdb range-scan "$OPS" "$RECORDS" "$KV_LSMDB_RANGE_TARGET_TRAVERSED_ENTRIES" "$KV_LSMDB_RANGE_MIN_OPS") || { echo "invalid KV range-scan policy" >&2; exit 2; }
TINY_TXN_OPS=$(kv_effective_ops redb tiny-txn "$OPS" "$RECORDS" "$KV_LSMDB_RANGE_TARGET_TRAVERSED_ENTRIES" "$KV_LSMDB_RANGE_MIN_OPS") || { echo "invalid KV tiny-txn policy" >&2; exit 2; }
KV_IMPORTED_FROM_RUN=${KV_IMPORTED_FROM_RUN:-}
KV_IMPORTED_CASES_MANIFEST=${KV_IMPORTED_CASES_MANIFEST:-}
IMPORTED_CASES_MANIFEST_SHA=""
if [[ -n "$KV_IMPORTED_CASES_MANIFEST" ]]; then
  [[ -n "$KV_IMPORTED_FROM_RUN" ]] || { echo "KV_IMPORTED_FROM_RUN is required with KV_IMPORTED_CASES_MANIFEST" >&2; exit 2; }
  [[ -s "$KV_IMPORTED_CASES_MANIFEST" ]] || { echo "import manifest missing or empty: $KV_IMPORTED_CASES_MANIFEST" >&2; exit 2; }
  IMPORTED_CASES_MANIFEST_SHA=$(sha256sum "$KV_IMPORTED_CASES_MANIFEST" | awk '{print $1}')
fi
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-kv-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,noise} "$DATA_DIR"

free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
if (( free_bytes < min_free_bytes )); then
  echo "refusing KV run: free=$free_bytes required=$min_free_bytes" >&2
  exit 75
fi

[[ -s "$RUN_DIR/host-start.txt" ]] || "$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"; BUILD_PROFILE=external
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
elif [[ "$PROFILE" == smoke ]]; then
  BIN="$TARGET_DIR/debug/kvbench"; BUILD_PROFILE=debug
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --locked --features kv-all --bin kvbench || exit $?
else
  BIN="$TARGET_DIR/release/kvbench"; BUILD_PROFILE=release
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features kv-all --bin kvbench || exit $?
fi
BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}')
RUNNER_SHA=$(sha256sum "$ROOT/scripts/run-kv-matrix.sh" | awk '{print $1}')
POLICY_SHA=$(sha256sum "$ROOT/scripts/kv-matrix-policy.sh" | awk '{print $1}')
NOISE_SHA=$(sha256sum "$ROOT/scripts/check-external-noise.py" | awk '{print $1}')
HOST_NAME=$(hostname)
MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FILESYSTEM=$(findmnt -n -o FSTYPE --target "$DATA_DIR")
SOURCE=$(findmnt -n -o SOURCE --target "$DATA_DIR")

ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
WORKLOADS=(point-read range-scan read-heavy balanced tiny-txn write-burst churn)
DURABILITIES=(relaxed sync)
[[ -n "${ENGINES_OVERRIDE:-}" ]] && read -r -a ENGINES <<< "$ENGINES_OVERRIDE"
[[ -n "${WORKLOADS_OVERRIDE:-}" ]] && read -r -a WORKLOADS <<< "$WORKLOADS_OVERRIDE"
[[ -n "${DURABILITIES_OVERRIDE:-}" ]] && read -r -a DURABILITIES <<< "$DURABILITIES_OVERRIDE"
if [[ "${MATRIX_RESUME_SHUFFLE_REMAINING:-0}" == 1 ]]; then RESUME_ORDER_POLICY=reshuffle-remaining; else RESUME_ORDER_POLICY=fixed-initial; fi

check_io_quiet() {
  [[ "$PROFILE" == smoke || "${ALLOW_BUSY:-0}" == 1 ]] && return 0
  local phase=$1 io_psi10
  io_psi10=$(awk '/^full / {for(i=1;i<=NF;i++) if($i ~ /^avg10=/){split($i,a,"="); print a[2]}}' /proc/pressure/io)
  if ! awk -v p="${io_psi10:-0}" 'BEGIN { exit !(p <= 5.0) }'; then
    echo "refusing KV performance case under I/O pressure ($phase): io PSI full avg10=${io_psi10}%" >&2
    return 75
  fi
}
check_external_noise() {
  local phase=$1 evidence=$2 rc
  [[ "$PROFILE" == smoke || "${ALLOW_EXTERNAL_NOISE:-0}" == 1 ]] && return 0
  if uv run --script "$ROOT/scripts/check-external-noise.py" --json-out "$evidence"; then rc=0; else rc=$?; fi
  if (( rc != 0 )); then
    echo "refusing KV performance case due to external host work ($phase): $(cat "$evidence" 2>/dev/null)" >&2
  fi
  return "$rc"
}

JOBS=()
for trial in $(seq 1 "$TRIALS"); do
  for durability in "${DURABILITIES[@]}"; do
    for workload in "${WORKLOADS[@]}"; do
      for engine in "${ENGINES[@]}"; do
        [[ "$engine" == lkv && "$durability" == relaxed ]] && continue
        [[ "$engine" == manifold && "$durability" == relaxed ]] && continue
        [[ "$engine" == jammdb && "$durability" == relaxed ]] && continue
        [[ "$engine" == lsmdb && "$durability" == relaxed ]] && continue
        [[ "$durability" == sync && ( "$engine" == paritydb-hash || "$engine" == paritydb-btree ) ]] && continue
        [[ "$engine" == lkv && "$workload" == range-scan ]] && continue
        [[ "$engine" == paritydb-hash && "$workload" == range-scan ]] && continue
        JOBS+=("$trial|$engine|$durability|$workload")
      done
    done
  done
done

SUPPORT_NEW="$RUN_DIR/support.json.new"
uv run --no-project python - "$SUPPORT_NEW" <<PY_SUPPORT
import json, sys
json.dump({
  "lane":"kv", "profile":"$PROFILE", "trials":$TRIALS,
  "records":$RECORDS, "ops":$OPS, "value_bytes":256, "txn_size":100, "scan_len":100,
  "lsmdb_range_scan_ops":$LSMDB_RANGE_OPS,
  "lsmdb_range_target_traversed_entries":$KV_LSMDB_RANGE_TARGET_TRAVERSED_ENTRIES,
  "lsmdb_range_min_ops":$KV_LSMDB_RANGE_MIN_OPS,
  "tiny_txn_ops":$TINY_TXN_OPS,
  "tiny_txn_divisor":10,
  "imported_from_run":"$KV_IMPORTED_FROM_RUN",
  "imported_cases_manifest_sha256":"$IMPORTED_CASES_MANIFEST_SHA",
  "engines":"${ENGINES[*]}", "workloads":"${WORKLOADS[*]}", "durabilities":"${DURABILITIES[*]}",
  "resume_order_policy":"$RESUME_ORDER_POLICY", "build_profile":"$BUILD_PROFILE",
  "benchmark_binary_sha256":"$BIN_SHA", "runner_sha256":"$RUNNER_SHA", "kv_matrix_policy_sha256":"$POLICY_SHA", "noise_guard_sha256":"$NOISE_SHA", "admission_policy":"pre-io+pre/post-external-v2",
  "hostname":"$HOST_NAME", "machine_id_sha256":"$MACHINE_ID_SHA256", "filesystem":"$FILESYSTEM", "source":"$SOURCE"
}, open(sys.argv[1], "w"), sort_keys=True, separators=(",",":"))
PY_SUPPORT
EXISTING_CASES=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
if [[ -s "$RUN_DIR/support.json" ]]; then
  if ! cmp -s "$RUN_DIR/support.json" "$SUPPORT_NEW"; then
    if (( EXISTING_CASES > 0 )); then rm -f "$SUPPORT_NEW"; echo "refusing KV resume: support identity changed" >&2; exit 2; fi
    mv "$SUPPORT_NEW" "$RUN_DIR/support.json"
  else rm -f "$SUPPORT_NEW"; fi
else mv "$SUPPORT_NEW" "$RUN_DIR/support.json"; fi

if [[ -s "$RUN_DIR/jobs.txt" ]]; then
  expected=$(mktemp); existing=$(mktemp)
  printf '%s\n' "${JOBS[@]}" | sort > "$expected"; sort "$RUN_DIR/jobs.txt" > "$existing"
  if ! cmp -s "$expected" "$existing"; then rm -f "$expected" "$existing"; echo "refusing KV resume: jobs.txt changed" >&2; exit 2; fi
  rm -f "$expected" "$existing"; mapfile -t ORDERED < "$RUN_DIR/jobs.txt"
else
  mapfile -t ORDERED < <(printf '%s\n' "${JOBS[@]}" | shuf); printf '%s\n' "${ORDERED[@]}" > "$RUN_DIR/jobs.txt"
fi
[[ "$RESUME_ORDER_POLICY" == reshuffle-remaining ]] && mapfile -t ORDERED < <(printf '%s\n' "${ORDERED[@]}" | shuf)
TOTAL=${#ORDERED[@]}

clear_failure() {
  local case_id=$1 failures="$RUN_DIR/failures.ndjson" tmp
  [[ -f "$failures" ]] || return 0
  tmp="${failures}.tmp"; jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' "$failures" > "$tmp"; mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}

INDEX=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1)); IFS='|' read -r trial engine durability workload <<< "$job"
  case_id="t${trial}-${engine}-${durability}-${workload}"
  out="$RUN_DIR/cases/$case_id.json"; err="$RUN_DIR/stderr/$case_id.log"
  noise_before="$RUN_DIR/noise/$case_id.before.json"; noise_after="$RUN_DIR/noise/$case_id.after.json"
  if [[ -s "$out" ]]; then clear_failure "$case_id"; continue; fi
  echo "[$INDEX/$TOTAL] $case_id" >&2
  check_io_quiet "before:$case_id" || exit $?
  check_external_noise "before:$case_id" "$noise_before" || exit $?
  rm -f "$out"
  CASE_OPS=$(kv_effective_ops "$engine" "$workload" "$OPS" "$RECORDS" "$KV_LSMDB_RANGE_TARGET_TRAVERSED_ENTRIES" "$KV_LSMDB_RANGE_MIN_OPS") || { echo "invalid KV case policy for $case_id" >&2; exit 2; }
  "$BIN" --engine "$engine" --durability "$durability" --workload "$workload" \
    --records "$RECORDS" --ops "$CASE_OPS" --value-bytes 256 --txn-size 100 --scan-len 100 \
    --trial "$trial" --seed 1592606758 --scenario baseline-core --root "$DATA_DIR" --output "$out" 2>"$err"
  rc=$?
  noise_rc=0; check_external_noise "after:$case_id" "$noise_after" || noise_rc=$?
  if (( noise_rc != 0 )); then
    rm -f "$out"; clear_failure "$case_id"
    exit "$noise_rc"
  fi
  if (( rc != 0 )) || [[ ! -s "$out" ]] || [[ $(wc -l < "$out") -ne 1 ]]; then
    rm -f "$out"; clear_failure "$case_id"
    jq -cn --arg case_id "$case_id" --arg stderr "$err" --argjson rc "$rc" '{case_id:$case_id,returncode:$rc,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
  else
    clear_failure "$case_id"; [[ -s "$err" ]] || rm -f "$err"
  fi
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
if uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" --expect-trials "$TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"; then summary_rc=0; else summary_rc=$?; fi
if [[ -s "$RUN_DIR/failures.ndjson" ]]; then FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson"); else FAILURES=0; fi
COMPLETED=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
printf 'run=%s total=%s completed=%s failures=%s summary_rc=%s results=%s\n' "$RUN_ID" "$TOTAL" "$COMPLETED" "$FAILURES" "$summary_rc" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 || summary_rc != 0 || COMPLETED != TOTAL ? 1 : 0 ))
