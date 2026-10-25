#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke)
    RECORDS=200; OPS=100; TRIALS=1; MIN_FREE_GIB=2
    PAYLOAD_SIZES=(512)
    PATTERNS=(hex-random random-ascii zeros repeated structured-text)
    ;;
  quick)
    RECORDS=20000; OPS=5000; TRIALS=1; MIN_FREE_GIB=10
    PAYLOAD_SIZES=(512 4096)
    PATTERNS=(hex-random random-ascii zeros repeated structured-text)
    ;;
  full)
    RECORDS=100000; OPS=20000; TRIALS=3; MIN_FREE_GIB=30
    PAYLOAD_SIZES=(512 4096 16384)
    PATTERNS=(hex-random random-ascii zeros repeated structured-text)
    ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac

ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
# shellcheck source=performance-runner-common.sh
source "$ROOT/scripts/performance-runner-common.sh"
# Consumed by sourced performance-runner-common.sh.
# shellcheck disable=SC2034
CASE_MIN_FREE_GIB=${PERFORMANCE_MIN_FREE_GIB:-$MIN_FREE_GIB}
TRIALS=${RECORD_COMPRESSION_TRIALS_OVERRIDE:-$TRIALS}
RECORDS=${RECORD_COMPRESSION_RECORDS_OVERRIDE:-$RECORDS}
OPS=${RECORD_COMPRESSION_OPS_OVERRIDE:-$OPS}

if [[ "$PROFILE" != smoke && -z "${DBBENCH_STORAGE_PROFILE_ID:-}" ]]; then
  echo "quick/full record compression campaigns require run-storage-envelope.sh and a storage profile" >&2
  exit 2
fi

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-record-compression-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="${DBBENCH_STORAGE_ROOT:-$ROOT}/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,raw,stderr,noise,storage,footprints} "$DATA_DIR"

free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
if (( free_bytes < min_free_bytes )); then
  echo "refusing record compression run: free=$free_bytes required=$min_free_bytes path=$DATA_DIR" >&2
  exit 75
fi

[[ -s "$RUN_DIR/host-start.txt" ]] || "$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT" || exit $?
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
BENCHCTL_TARGET_DIR=${BENCHCTL_TARGET_DIR:-$TARGET_DIR}
BENCHCTL="$BENCHCTL_TARGET_DIR/debug/all-db-benchctl"
CARGO_TARGET_DIR="$BENCHCTL_TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --locked \
  --manifest-path "$ROOT/tools/benchctl/Cargo.toml" --bin all-db-benchctl || exit $?
BENCHCTL_SHA=$(sha256sum "$BENCHCTL" | awk '{print $1}')
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"; BUILD_PROFILE=external
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
else
  "$ROOT/scripts/ensure-sqlite-3.54.0.sh" || exit $?
  if [[ "$PROFILE" == smoke ]]; then
    BIN="$TARGET_DIR/debug/recordbench"; BUILD_PROFILE=debug
    CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --locked --features record --bin recordbench || exit $?
  else
    BIN="$TARGET_DIR/release/recordbench"; BUILD_PROFILE=release
    CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features record --bin recordbench || exit $?
  fi
fi

ROCKS_TARGET_DIR=${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}
if [[ -n "${ROCKS_BENCH_BIN:-}" ]]; then
  ROCKS_BIN="$ROCKS_BENCH_BIN"; ROCKS_BUILD_PROFILE=external
  [[ -x "$ROCKS_BIN" ]] || { echo "ROCKS_BENCH_BIN is not executable: $ROCKS_BIN" >&2; exit 2; }
else
  if [[ "$PROFILE" == smoke ]]; then
    ROCKS_BIN="$ROCKS_TARGET_DIR/debug/surrealdb-rocksdb-recordbench"; ROCKS_BUILD_PROFILE=debug
    "$ROOT/scripts/cargo-local-1.99.sh" build --locked --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" --target-dir "$ROCKS_TARGET_DIR" --bin surrealdb-rocksdb-recordbench || exit $?
  else
    ROCKS_BIN="$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordbench"; ROCKS_BUILD_PROFILE=release
    "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" --target-dir "$ROCKS_TARGET_DIR" --bin surrealdb-rocksdb-recordbench || exit $?
  fi
fi

BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}')
ROCKS_SHA=$(sha256sum "$ROCKS_BIN" | awk '{print $1}')
HARNESS_COMMIT=$(git -C "$ROOT" rev-parse HEAD)
if [[ "$BUILD_PROFILE" == external ]]; then BENCH_SOURCE_COMMIT=${BENCH_SOURCE_COMMIT:-}; else BENCH_SOURCE_COMMIT=${BENCH_SOURCE_COMMIT:-$HARNESS_COMMIT}; fi
if [[ "$ROCKS_BUILD_PROFILE" == external ]]; then ROCKS_BENCH_SOURCE_COMMIT=${ROCKS_BENCH_SOURCE_COMMIT:-}; else ROCKS_BENCH_SOURCE_COMMIT=${ROCKS_BENCH_SOURCE_COMMIT:-$HARNESS_COMMIT}; fi
[[ "$BENCH_SOURCE_COMMIT" =~ ^[0-9a-f]{40}$ ]] || { echo "invalid or missing BENCH_SOURCE_COMMIT=$BENCH_SOURCE_COMMIT" >&2; exit 2; }
[[ "$ROCKS_BENCH_SOURCE_COMMIT" =~ ^[0-9a-f]{40}$ ]] || { echo "invalid or missing ROCKS_BENCH_SOURCE_COMMIT=$ROCKS_BENCH_SOURCE_COMMIT" >&2; exit 2; }

RUNNER_SHA=$(sha256sum "$ROOT/scripts/run-record-compression-matrix.sh" | awk '{print $1}')
COUNTERS_SHA=$(sha256sum "$ROOT/scripts/capture-storage-counters.sh" | awk '{print $1}')
SETTLE_SHA=$(sha256sum "$ROOT/scripts/settle-storage-path.sh" | awk '{print $1}')
COMMON_SHA=$(sha256sum "$ROOT/scripts/performance-runner-common.sh" | awk '{print $1}')
NOISE_SHA=$(sha256sum "$ROOT/tools/benchctl/src/noise.rs" | awk '{print $1}')
HOST_NAME=$(hostname)
MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FILESYSTEM=$(findmnt -n -o FSTYPE --target "$DATA_DIR")
SOURCE=$(findmnt -n -o SOURCE --target "$DATA_DIR")

ENGINES=(surrealdb turso sqlite surrealdb-rocksdb)
WORKLOADS=(write-burst point-read)
[[ -n "${ENGINES_OVERRIDE:-}" ]] && read -r -a ENGINES <<< "$ENGINES_OVERRIDE"
[[ -n "${WORKLOADS_OVERRIDE:-}" ]] && read -r -a WORKLOADS <<< "$WORKLOADS_OVERRIDE"
[[ -n "${PAYLOAD_SIZES_OVERRIDE:-}" ]] && read -r -a PAYLOAD_SIZES <<< "$PAYLOAD_SIZES_OVERRIDE"
[[ -n "${PATTERNS_OVERRIDE:-}" ]] && read -r -a PATTERNS <<< "$PATTERNS_OVERRIDE"

JOBS=()
for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    for workload in "${WORKLOADS[@]}"; do
      for payload_bytes in "${PAYLOAD_SIZES[@]}"; do
        for pattern in "${PATTERNS[@]}"; do
          JOBS+=("$trial|$engine|$workload|$payload_bytes|$pattern")
        done
      done
    done
  done
done
(( ${#JOBS[@]} > 0 )) || { echo "record compression matrix contains no cases" >&2; exit 2; }

PROFILE_SHA=${DBBENCH_STORAGE_PROFILE_SHA256:-}
PROFILE_ID=${DBBENCH_STORAGE_PROFILE_ID:-unprofiled-smoke}
PROFILE_MODE=${DBBENCH_STORAGE_PROFILE_MODE:-unprofiled}
SUPPORT_NEW="$RUN_DIR/support.json.new"
jq -n \
  --arg lane record-compression --arg profile "$PROFILE" --arg host "$HOST_NAME" \
  --arg machine "$MACHINE_ID_SHA256" --arg fs "$FILESYSTEM" --arg source "$SOURCE" \
  --arg build "$BUILD_PROFILE" --arg binary "$BIN_SHA" --arg benchmark_source_commit "$BENCH_SOURCE_COMMIT" \
  --arg rocks_build "$ROCKS_BUILD_PROFILE" --arg rocks_binary "$ROCKS_SHA" --arg rocks_source_commit "$ROCKS_BENCH_SOURCE_COMMIT" \
  --arg harness_commit "$HARNESS_COMMIT" --arg runner "$RUNNER_SHA" \
  --arg benchctl "$BENCHCTL_SHA" --arg counters "$COUNTERS_SHA" --arg settle "$SETTLE_SHA" \
  --arg common "$COMMON_SHA" --arg noise "$NOISE_SHA" --arg storage_profile_id "$PROFILE_ID" \
  --arg storage_profile_mode "$PROFILE_MODE" --arg storage_profile_sha256 "$PROFILE_SHA" \
  --arg engines "${ENGINES[*]}" --arg workloads "${WORKLOADS[*]}" \
  --arg values "${PAYLOAD_SIZES[*]}" --arg patterns "${PATTERNS[*]}" \
  --argjson records "$RECORDS" --argjson ops "$OPS" --argjson trials "$TRIALS" --argjson case_count "${#JOBS[@]}" \
  '{format_version:1,lane:$lane,profile:$profile,records:$records,ops:$ops,trials:$trials,case_count:$case_count,
    engines:($engines|split(" ")),workloads:($workloads|split(" ")),payload_sizes:($values|split(" ")|map(tonumber)),
    payload_patterns:($patterns|split(" ")),durability:"sync",product_compression:"product-default",
    build_profile:$build,benchmark_binary_sha256:$binary,benchmark_source_commit:$benchmark_source_commit,
    rocks_build_profile:$rocks_build,surrealdb_rocksdb_binary_sha256:$rocks_binary,
    surrealdb_rocksdb_source_commit:$rocks_source_commit,harness_commit:$harness_commit,
    runner_sha256:$runner,benchctl_binary_sha256:$benchctl,storage_counter_helper_sha256:$counters,
    storage_settle_helper_sha256:$settle,storage_settle_protocol:"sync-before-baseline+sync-before-physical-snapshot+sync-after-cleanup-v1",
    performance_common_sha256:$common,continuous_noise_guard_sha256:$noise,
    hostname:$host,machine_id_sha256:$machine,filesystem:$fs,source:$source,
    storage_profile:{id:$storage_profile_id,mode:$storage_profile_mode,sha256:$storage_profile_sha256}}' > "$SUPPORT_NEW"
if [[ -s "$RUN_DIR/support.json" ]]; then
  if ! cmp -s "$RUN_DIR/support.json" "$SUPPORT_NEW"; then
    existing=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
    if (( existing != 0 )); then
      rm -f "$SUPPORT_NEW"
      echo "refusing record compression resume: support identity changed" >&2
      exit 2
    fi
    mv "$SUPPORT_NEW" "$RUN_DIR/support.json"
  else
    rm -f "$SUPPORT_NEW"
  fi
else
  mv "$SUPPORT_NEW" "$RUN_DIR/support.json"
fi

if [[ -s "$RUN_DIR/jobs.txt" ]]; then
  expected=$(mktemp); existing=$(mktemp)
  printf '%s\n' "${JOBS[@]}" | sort > "$expected"; sort "$RUN_DIR/jobs.txt" > "$existing"
  if ! cmp -s "$expected" "$existing"; then rm -f "$expected" "$existing"; echo "refusing record compression resume: jobs.txt changed" >&2; exit 2; fi
  rm -f "$expected" "$existing"; mapfile -t ORDERED < "$RUN_DIR/jobs.txt"
else
  mapfile -t ORDERED < <(printf '%s\n' "${JOBS[@]}" | shuf)
  printf '%s\n' "${ORDERED[@]}" > "$RUN_DIR/jobs.txt"
fi
TOTAL=${#ORDERED[@]}
if [[ "${MATRIX_PLAN_ONLY:-0}" == 1 ]]; then printf 'run=%s total=%s plan_only=1 support=%s\n' "$RUN_ID" "$TOTAL" "$RUN_DIR/support.json"; exit 0; fi

clean_data_dir() {
  find "$DATA_DIR" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
}

FAILURES=0
INDEX=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1))
  IFS='|' read -r trial engine workload payload_bytes pattern <<< "$job"
  case_id="t${trial}-${engine}-${workload}-p${payload_bytes}-pp${pattern}"
  out="$RUN_DIR/cases/$case_id.json"
  [[ -s "$out" ]] && continue
  raw="$RUN_DIR/raw/$case_id.json"; err="$RUN_DIR/stderr/$case_id.log"
  noise_before="$RUN_DIR/noise/$case_id.before.json"; noise_during="$RUN_DIR/noise/$case_id.during.json"; noise_after="$RUN_DIR/noise/$case_id.after.json"
  echo "[$INDEX/$TOTAL] $case_id" >&2

  performance_check_io_quiet "$PROFILE" "before:$case_id" || exit $?
  performance_check_external_noise "$ROOT" "$PROFILE" "before:$case_id" "$noise_before" || exit $?
  clean_data_dir; rm -f "$raw" "$out"
  "$ROOT/scripts/settle-storage-path.sh" "$DATA_DIR" || exit $?
  storage_before=$("$ROOT/scripts/capture-storage-counters.sh" "$DATA_DIR") || exit $?
  dataset_before=$("$BENCHCTL" storage-footprint "$DATA_DIR") || exit $?

  case_bin="$BIN"; case_engine="$engine"
  if [[ "$engine" == surrealdb-rocksdb ]]; then case_bin="$ROCKS_BIN"; case_engine=surrealdb-rocksdb; fi
  cmd=("$case_bin" --engine "$case_engine" --durability sync --workload "$workload"
    --records "$RECORDS" --ops "$OPS" --payload-bytes "$payload_bytes" --payload-pattern "$pattern"
    --txn-size 100 --trial "$trial" --seed 1592606758 --scenario record-compression-v1
    --root "$DATA_DIR" --keep-db --output "$raw")
  if [[ "$PROFILE" == smoke ]]; then "${cmd[@]}" 2>"$err"; else performance_run_with_continuous_noise "$ROOT" "$noise_during" "${cmd[@]}" 2>"$err"; fi
  rc=$?

  noise_rc=0
  performance_check_external_noise "$ROOT" "$PROFILE" "after:$case_id" "$noise_after" || noise_rc=$?
  if (( noise_rc != 0 )); then
    performance_preserve_noise_rejection "$RUN_DIR" "$case_id" post-external "$noise_before" "$noise_during" "$noise_after"
    clean_data_dir; rm -f "$raw" "$out"; "$ROOT/scripts/settle-storage-path.sh" "$DATA_DIR" || exit $?; exit "$noise_rc"
  fi
  if (( rc == 75 )) && performance_continuous_rejected "$noise_during"; then
    performance_preserve_noise_rejection "$RUN_DIR" "$case_id" continuous "$noise_before" "$noise_during" "$noise_after"
    clean_data_dir; rm -f "$raw" "$out"; "$ROOT/scripts/settle-storage-path.sh" "$DATA_DIR" || exit $?; exit 75
  fi
  if (( rc != 0 )) || [[ ! -s "$raw" ]] || [[ $(wc -l < "$raw") -ne 1 ]]; then
    FAILURES=$((FAILURES + 1))
    jq -cn --arg case_id "$case_id" --arg stderr "$err" --argjson rc "$rc" '{case_id:$case_id,returncode:$rc,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
    clean_data_dir; rm -f "$raw"; "$ROOT/scripts/settle-storage-path.sh" "$DATA_DIR" || exit $?; continue
  fi

  db_path=$(jq -er '.path | select(type=="string" and length>0)' "$raw") || exit $?
  data_real=$(realpath "$DATA_DIR") || exit $?
  db_real=$(realpath "$db_path") || exit $?
  case "$db_real" in "$data_real"/*) ;; *) echo "refusing unsafe record DB path: $db_real" >&2; exit 2 ;; esac
  "$ROOT/scripts/settle-storage-path.sh" "$DATA_DIR" || exit $?
  footprint=$("$BENCHCTL" storage-footprint "$db_real") || exit $?
  dataset_after=$("$BENCHCTL" storage-footprint "$DATA_DIR") || exit $?
  storage_after=$("$ROOT/scripts/capture-storage-counters.sh" "$DATA_DIR") || exit $?
  printf '%s\n' "$footprint" > "$RUN_DIR/footprints/$case_id.json"
  jq -cn --argjson before "$storage_before" --argjson after "$storage_after" '{before:$before,after:$after}' > "$RUN_DIR/storage/$case_id.json"

  foreground_records=0; [[ "$workload" == write-burst ]] && foreground_records=$OPS
  jq -c --argjson footprint "$footprint" --argjson dataset_before "$dataset_before" --argjson dataset_after "$dataset_after" \
    --argjson storage_before "$storage_before" --argjson storage_after "$storage_after" \
    --argjson prefill_record_bytes "$((RECORDS * (payload_bytes + 12)))" \
    --argjson foreground_record_bytes "$((foreground_records * (payload_bytes + 12)))" \
    '. + {record_source_lane:.lane,lane:"record-compression",compression_storage:{footprint:$footprint,
      dataset_snapshot:{before:$dataset_before,after:$dataset_after},storage_counters:{before:$storage_before,after:$storage_after},
      logical:{prefill_record_bytes:$prefill_record_bytes,foreground_record_bytes:$foreground_record_bytes,
        definition:"id(8) + bucket(4) + payload bytes; excludes database/document encoding and indexes"}}}' "$raw" > "$out"
  clean_data_dir; "$ROOT/scripts/settle-storage-path.sh" "$DATA_DIR" || exit $?
  [[ -s "$err" ]] || rm -f "$err"
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT" || exit $?
if "$BENCHCTL" summarize-compression "$RUN_DIR/results.ndjson" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"; then summary_rc=0; else summary_rc=$?; fi
[[ -s "$RUN_DIR/failures.ndjson" ]] && FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson")
COMPLETED=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
printf 'run=%s total=%s completed=%s failures=%s summary_rc=%s results=%s\n' "$RUN_ID" "$TOTAL" "$COMPLETED" "$FAILURES" "$summary_rc" "$RUN_DIR/results.ndjson"
(( FAILURES == 0 && summary_rc == 0 && COMPLETED == TOTAL ))
