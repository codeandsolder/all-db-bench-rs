#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke) RECORDS=50; OPS=50; TRIALS=1; MIN_FREE_GIB=2 ;;
  quick) RECORDS=10000; OPS=10000; TRIALS=3; MIN_FREE_GIB=10 ;;
  full) RECORDS=100000; OPS=50000; TRIALS=7; MIN_FREE_GIB=30 ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac
TRIALS=${RECORD_TRIALS_OVERRIDE:-$TRIALS}
RECORDS=${RECORD_RECORDS_OVERRIDE:-$RECORDS}
OPS=${RECORD_OPS_OVERRIDE:-$OPS}
if (( OPS <= 100 )); then TINY_TXN_OPS=$OPS; else TINY_TXN_OPS=$((OPS / 2)); fi
ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
# shellcheck source=record-result-cleanup.sh
source "$ROOT/scripts/record-result-cleanup.sh"
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-record-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"; DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,noise} "$DATA_DIR"
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' '); min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
(( free_bytes >= min_free_bytes )) || { echo "refusing record run: free=$free_bytes required=$min_free_bytes" >&2; exit 75; }
"$ROOT/scripts/ensure-sqlite-3.53.4.sh"
[[ -s "$RUN_DIR/host-start.txt" ]] || "$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then BIN="$BENCH_BIN"; BUILD_PROFILE=external; [[ -x "$BIN" ]] || exit 2
elif [[ "$PROFILE" == smoke ]]; then BIN="$TARGET_DIR/debug/recordbench"; BUILD_PROFILE=debug; CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --locked --features record --bin recordbench || exit $?
else BIN="$TARGET_DIR/release/recordbench"; BUILD_PROFILE=release; CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features record --bin recordbench || exit $?; fi
ROCKS_TARGET_DIR=${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}
if [[ -n "${ROCKS_BENCH_BIN:-}" ]]; then ROCKS_BIN="$ROCKS_BENCH_BIN"; ROCKS_BUILD_PROFILE=external; [[ -x "$ROCKS_BIN" ]] || exit 2
elif [[ "$PROFILE" == smoke ]]; then ROCKS_BIN="$ROCKS_TARGET_DIR/debug/surrealdb-rocksdb-recordbench"; ROCKS_BUILD_PROFILE=debug; "$ROOT/scripts/cargo-local-1.99.sh" build --locked --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" --target-dir "$ROCKS_TARGET_DIR" || exit $?
else ROCKS_BIN="$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordbench"; ROCKS_BUILD_PROFILE=release; "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" --target-dir "$ROCKS_TARGET_DIR" || exit $?; fi
BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}'); ROCKS_SHA=$(sha256sum "$ROCKS_BIN" | awk '{print $1}')
RUNNER_SHA=$(sha256sum "$ROOT/scripts/run-record-matrix.sh" | awk '{print $1}'); NOISE_SHA=$(sha256sum "$ROOT/scripts/check-external-noise.py" | awk '{print $1}')
HOST_NAME=$(hostname); MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}'); FILESYSTEM=$(findmnt -n -o FSTYPE --target "$DATA_DIR"); SOURCE=$(findmnt -n -o SOURCE --target "$DATA_DIR")
READ_MATERIALIZATION=full-record-v1
WRITE_MATERIALIZATION=no-return-v1
ENGINES=(surrealdb turso sqlite surrealdb-rocksdb); WORKLOADS=(point-read indexed-read read-heavy tiny-txn write-burst); DURABILITIES=(relaxed sync)
[[ -n "${ENGINES_OVERRIDE:-}" ]] && read -r -a ENGINES <<< "$ENGINES_OVERRIDE"; [[ -n "${WORKLOADS_OVERRIDE:-}" ]] && read -r -a WORKLOADS <<< "$WORKLOADS_OVERRIDE"; [[ -n "${DURABILITIES_OVERRIDE:-}" ]] && read -r -a DURABILITIES <<< "$DURABILITIES_OVERRIDE"
if [[ "${MATRIX_RESUME_SHUFFLE_REMAINING:-0}" == 1 ]]; then RESUME_ORDER_POLICY=reshuffle-remaining; else RESUME_ORDER_POLICY=fixed-initial; fi
check_free_space(){ [[ "$PROFILE" == smoke ]] && return 0; local phase=$1 min_gib=${PERFORMANCE_MIN_FREE_GIB:-8} free_kib min_kib; free_kib=$(df -Pk -- "$DATA_DIR" | awk 'NR==2 {print $4}')||return 2; min_kib=$(awk -v g="$min_gib" 'BEGIN{if(g<0)exit 2;printf "%.0f",g*1024*1024}')||return 2; ((free_kib>=min_kib))||{ echo "refusing record performance case under low free space ($phase): free_kib=$free_kib required_kib=$min_kib path=$DATA_DIR" >&2; return 75; }; }
check_io_quiet(){ [[ "$PROFILE" == smoke ]] && return 0; local phase=$1 p; check_free_space "$phase"||return $?; [[ "${ALLOW_BUSY:-0}" == 1 ]]&&return 0; p=$(awk '/^full / {for(i=1;i<=NF;i++) if($i ~ /^avg10=/){split($i,a,"="); print a[2]}}' /proc/pressure/io); awk -v p="${p:-0}" 'BEGIN{exit !(p<=5.0)}' || { echo "refusing record performance case under I/O pressure ($phase): ${p}%" >&2; return 75; }; }
check_external_noise(){ local phase=$1 evidence=$2 rc; [[ "$PROFILE" == smoke || "${ALLOW_EXTERNAL_NOISE:-0}" == 1 ]] && return 0; if uv run --script "$ROOT/scripts/check-external-noise.py" --json-out "$evidence"; then rc=0; else rc=$?; fi; ((rc==0)) || echo "refusing record performance case due to external host work ($phase): $(cat "$evidence" 2>/dev/null)" >&2; return "$rc"; }
JOBS=(); for trial in $(seq 1 "$TRIALS"); do for engine in "${ENGINES[@]}"; do for durability in "${DURABILITIES[@]}"; do for workload in "${WORKLOADS[@]}"; do JOBS+=("$trial|$engine|$durability|$workload"); done; done; done; done
SUPPORT_NEW="$RUN_DIR/support.json.new"
uv run --no-project python - "$SUPPORT_NEW" <<PY_SUPPORT
import json,sys
json.dump({"lane":"record","profile":"$PROFILE","trials":$TRIALS,"records":$RECORDS,"ops":$OPS,"tiny_txn_ops":$TINY_TXN_OPS,"payload_bytes":512,"txn_size":100,"engines":"${ENGINES[*]}","workloads":"${WORKLOADS[*]}","durabilities":"${DURABILITIES[*]}","resume_order_policy":"$RESUME_ORDER_POLICY","build_profile":"$BUILD_PROFILE","rocks_build_profile":"$ROCKS_BUILD_PROFILE","benchmark_binary_sha256":"$BIN_SHA","surrealdb_rocksdb_binary_sha256":"$ROCKS_SHA","read_materialization":"$READ_MATERIALIZATION","write_materialization":"$WRITE_MATERIALIZATION","runner_sha256":"$RUNNER_SHA","noise_guard_sha256":"$NOISE_SHA","admission_policy":"pre-io+pre/post-external-v2","hostname":"$HOST_NAME","machine_id_sha256":"$MACHINE_ID_SHA256","filesystem":"$FILESYSTEM","source":"$SOURCE"},open(sys.argv[1],"w"),sort_keys=True,separators=(",",":"))
PY_SUPPORT
EXISTING_CASES=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
if [[ -s "$RUN_DIR/support.json" ]]; then if ! cmp -s "$RUN_DIR/support.json" "$SUPPORT_NEW"; then if ((EXISTING_CASES>0)); then rm -f "$SUPPORT_NEW"; echo "refusing record resume: support identity changed" >&2; exit 2; fi; mv "$SUPPORT_NEW" "$RUN_DIR/support.json"; else rm -f "$SUPPORT_NEW"; fi; else mv "$SUPPORT_NEW" "$RUN_DIR/support.json"; fi
if [[ -s "$RUN_DIR/jobs.txt" ]]; then expected=$(mktemp); existing=$(mktemp); printf '%s\n' "${JOBS[@]}"|sort>"$expected"; sort "$RUN_DIR/jobs.txt">"$existing"; cmp -s "$expected" "$existing" || { rm -f "$expected" "$existing"; echo "refusing record resume: jobs.txt changed" >&2; exit 2; }; rm -f "$expected" "$existing"; mapfile -t ORDERED < "$RUN_DIR/jobs.txt"; else mapfile -t ORDERED < <(printf '%s\n' "${JOBS[@]}"|shuf); printf '%s\n' "${ORDERED[@]}">"$RUN_DIR/jobs.txt"; fi
[[ "$RESUME_ORDER_POLICY" == reshuffle-remaining ]] && mapfile -t ORDERED < <(printf '%s\n' "${ORDERED[@]}"|shuf); TOTAL=${#ORDERED[@]}
clear_failure(){ local case_id=$1 failures="$RUN_DIR/failures.ndjson" tmp; [[ -f "$failures" ]]||return 0; tmp="${failures}.tmp"; jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' "$failures">"$tmp"; mv "$tmp" "$failures"; [[ -s "$failures" ]]||rm -f "$failures"; }
INDEX=0
for job in "${ORDERED[@]}"; do INDEX=$((INDEX+1)); IFS='|' read -r trial engine durability workload <<< "$job"; case_id="t${trial}-${engine}-${durability}-${workload}"; out="$RUN_DIR/cases/$case_id.json"; err="$RUN_DIR/stderr/$case_id.log"; noise_before="$RUN_DIR/noise/$case_id.before.json"; noise_after="$RUN_DIR/noise/$case_id.after.json"; if [[ -s "$out" ]]; then clear_failure "$case_id"; continue; fi; echo "[$INDEX/$TOTAL] $case_id" >&2; check_io_quiet "before:$case_id"||exit $?; check_external_noise "before:$case_id" "$noise_before"||exit $?; rm -f "$out"; CASE_BIN="$BIN"; [[ "$engine" == surrealdb-rocksdb ]]&&CASE_BIN="$ROCKS_BIN"; CASE_OPS=$OPS; [[ "$workload" == tiny-txn ]] && CASE_OPS=$TINY_TXN_OPS; "$CASE_BIN" --engine "$engine" --durability "$durability" --workload "$workload" --records "$RECORDS" --ops "$CASE_OPS" --payload-bytes 512 --txn-size 100 --trial "$trial" --seed 1592606758 --scenario baseline-core --root "$DATA_DIR" --output "$out" --keep-db 2>"$err"; rc=$?; noise_rc=0; check_external_noise "after:$case_id" "$noise_after"||noise_rc=$?; if ((noise_rc!=0)); then if [[ -s "$out" ]]; then record_cleanup_result_db "$out" "$DATA_DIR" || true; fi; rm -f "$out"; clear_failure "$case_id"; exit "$noise_rc"; fi; if ((rc==0)) && [[ -s "$out" ]] && [[ $(wc -l < "$out") -eq 1 ]]; then record_cleanup_result_db "$out" "$DATA_DIR" || rc=$?; fi; if ((rc==0)) && [[ -s "$out" ]] && [[ $(wc -l < "$out") -eq 1 ]]; then if ! jq -e --arg read_expected "$READ_MATERIALIZATION" --arg write_expected "$WRITE_MATERIALIZATION" '.read_materialization == $read_expected and .write_materialization == $write_expected' "$out" >/dev/null; then rc=2; printf 'record result semantic identity mismatch: expected read_materialization=%s write_materialization=%s\n' "$READ_MATERIALIZATION" "$WRITE_MATERIALIZATION" >>"$err"; fi; fi; if ((rc!=0))||[[ ! -s "$out" ]]||[[ $(wc -l < "$out") -ne 1 ]]; then rm -f "$out"; clear_failure "$case_id"; jq -cn --arg case_id "$case_id" --arg stderr "$err" --argjson rc "$rc" '{case_id:$case_id,returncode:$rc,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"; else clear_failure "$case_id"; [[ -s "$err" ]]||rm -f "$err"; fi; done
find "$RUN_DIR/cases" -type f -name '*.json' -print0|sort -z|xargs -0 -r cat>"$RUN_DIR/results.ndjson"; "$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"; if uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" --expect-trials "$TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"; then summary_rc=0; else summary_rc=$?; fi; if [[ -s "$RUN_DIR/failures.ndjson" ]];then FAILURES=$(wc -l<"$RUN_DIR/failures.ndjson");else FAILURES=0;fi; COMPLETED=$(find "$RUN_DIR/cases" -type f -name '*.json'|wc -l); printf 'run=%s total=%s completed=%s failures=%s summary_rc=%s results=%s\n' "$RUN_ID" "$TOTAL" "$COMPLETED" "$FAILURES" "$summary_rc" "$RUN_DIR/results.ndjson"; exit $((FAILURES>0||summary_rc!=0||COMPLETED!=TOTAL?1:0))
