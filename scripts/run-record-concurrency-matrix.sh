#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke)
    TRIALS=1; RECORDS=2000; OPS=4000; PAYLOAD=128; TXN=100
    CLIENTS=(1 4 8); WORKLOADS=(point-read read-heavy write-burst)
    RELAXED_CLIENTS=(1 8); STRESS_CLIENTS=(1 8); TX_CLIENTS=(1 8)
    STRESS_PAYLOAD=2048; TX1_OPS=2000; TX1000_OPS=8000; HOT_RECORDS=64; HOT_OPS=4000
    ;;
  quick)
    TRIALS=3; RECORDS=10000; OPS=24000; PAYLOAD=512; TXN=100
    CLIENTS=(1 2 4 8); WORKLOADS=(point-read indexed-read read-heavy tiny-txn write-burst)
    RELAXED_CLIENTS=(1 4 8); STRESS_CLIENTS=(1 4 8); TX_CLIENTS=(1 4 8)
    STRESS_PAYLOAD=4096; TX1_OPS=24000; TX1000_OPS=24000; HOT_RECORDS=64; HOT_OPS=24000
    ;;
  full)
    TRIALS=5; RECORDS=100000; OPS=80000; PAYLOAD=512; TXN=100
    CLIENTS=(1 2 4 8 16); WORKLOADS=(point-read indexed-read read-heavy tiny-txn write-burst)
    RELAXED_CLIENTS=(1 4 8 16); STRESS_CLIENTS=(1 4 8 16); TX_CLIENTS=(1 4 8 16)
    STRESS_PAYLOAD=16384; TX1_OPS=80000; TX1000_OPS=80000; HOT_RECORDS=64; HOT_OPS=80000
    ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
CPUS=$(getconf _NPROCESSORS_ONLN)
check_quiet_host() {
  local phase=${1:-start} load1 io_psi10
  load1=$(awk '{print $1}' /proc/loadavg)
  io_psi10=$(awk '/^full / {for(i=1;i<=NF;i++) if($i ~ /^avg10=/){split($i,a,"="); print a[2]}}' /proc/pressure/io)
  if ! awk -v l="$load1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 1.0) }'; then
    echo "refusing record concurrency benchmark on busy host ($phase): load1=$load1 visible_cpus=$CPUS" >&2
    return 75
  fi
  if ! awk -v p="${io_psi10:-0}" 'BEGIN { exit !(p <= 10.0) }'; then
    echo "refusing record concurrency benchmark under I/O pressure ($phase): io PSI full avg10=${io_psi10}%" >&2
    return 75
  fi
}
if [[ "${ALLOW_BUSY:-0}" != 1 ]]; then
  check_quiet_host start || exit $?
fi

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-record-concurrency-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr} "$DATA_DIR"
"$ROOT/scripts/ensure-sqlite-3.53.4.sh"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"
  BUILD_PROFILE="external"
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
elif [[ "$PROFILE" == smoke ]]; then
  BIN="$TARGET_DIR/debug/recordconcurrency"
  BUILD_PROFILE="debug"
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" \
    build --locked --features record --bin recordconcurrency || exit $?
else
  BIN="$TARGET_DIR/release/recordconcurrency"
  BUILD_PROFILE="release"
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" \
    build --release --locked --features record --bin recordconcurrency || exit $?
fi
BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}')

ROCKS_TARGET_DIR=${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}
if [[ -n "${ROCKS_BENCH_BIN:-}" ]]; then
  ROCKS_BIN="$ROCKS_BENCH_BIN"
  ROCKS_BUILD_PROFILE="external"
  [[ -x "$ROCKS_BIN" ]] || { echo "ROCKS_BENCH_BIN is not executable: $ROCKS_BIN" >&2; exit 2; }
elif [[ "$PROFILE" == smoke ]]; then
  ROCKS_BIN="$ROCKS_TARGET_DIR/debug/surrealdb-rocksdb-recordconcurrency"
  ROCKS_BUILD_PROFILE="debug"
  "$ROOT/scripts/cargo-local-1.99.sh" build --locked \
    --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" \
    --target-dir "$ROCKS_TARGET_DIR" --bin surrealdb-rocksdb-recordconcurrency || exit $?
else
  ROCKS_BIN="$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordconcurrency"
  ROCKS_BUILD_PROFILE="release"
  "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked \
    --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" \
    --target-dir "$ROCKS_TARGET_DIR" --bin surrealdb-rocksdb-recordconcurrency || exit $?
fi
ROCKS_BIN_SHA=$(sha256sum "$ROCKS_BIN" | awk '{print $1}')
if [[ "${ALLOW_BUSY:-0}" != 1 ]]; then
  check_quiet_host post-build || {
    rc=$?
    echo "benchmark binaries are now built; rerun once the host is quiet" >&2
    exit "$rc"
  }
fi

ENGINES=(surrealdb turso sqlite surrealdb-rocksdb)
if [[ -n "${ENGINES_OVERRIDE:-}" ]]; then
  read -r -a ENGINES <<< "$ENGINES_OVERRIDE"
fi
JOBS=()
add() { JOBS+=("$1|$2|$3|$4|$5|$6|$7|$8|$9|${10}"); }
for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    for clients in "${CLIENTS[@]}"; do
      for workload in "${WORKLOADS[@]}"; do
        add record-concurrency-core "$engine" sync "$workload" "$clients" "$RECORDS" "$OPS" "$PAYLOAD" "$TXN" "$trial"
      done
    done

    for clients in "${RELAXED_CLIENTS[@]}"; do
      for workload in read-heavy write-burst; do
        add record-concurrency-relaxed "$engine" relaxed "$workload" "$clients" "$RECORDS" "$OPS" "$PAYLOAD" "$TXN" "$trial"
      done
    done

    for clients in "${STRESS_CLIENTS[@]}"; do
      for workload in read-heavy write-burst; do
        add record-concurrency-large-payload "$engine" sync "$workload" "$clients" "$RECORDS" "$OPS" "$STRESS_PAYLOAD" "$TXN" "$trial"
      done
    done

    for clients in "${STRESS_CLIENTS[@]}"; do
      add record-concurrency-hotset "$engine" sync read-heavy "$clients" "$HOT_RECORDS" "$HOT_OPS" "$PAYLOAD" "$TXN" "$trial"
    done

    for clients in "${TX_CLIENTS[@]}"; do
      add record-concurrency-txn-1 "$engine" sync write-burst "$clients" "$RECORDS" "$TX1_OPS" "$PAYLOAD" 1 "$trial"
      add record-concurrency-txn-1000 "$engine" sync write-burst "$clients" "$RECORDS" "$TX1000_OPS" "$PAYLOAD" 1000 "$trial"
    done
  done
done

mapfile -t ORDERED < <(printf '%s\n' "${JOBS[@]}" | shuf)
TOTAL=${#ORDERED[@]}
printf '%s\n' "${ORDERED[@]}" > "$RUN_DIR/jobs.txt"
cat > "$RUN_DIR/support.json" <<JSON
{
  "lane": "record-concurrency",
  "profile": "$PROFILE",
  "trials": $TRIALS,
  "case_count": $TOTAL,
  "build_profile": "$BUILD_PROFILE",
  "benchmark_binary_sha256": "$BIN_SHA",
  "benchmark_binary_build_profile": "$BUILD_PROFILE",
  "surrealdb_rocksdb_binary_sha256": "$ROCKS_BIN_SHA",
  "surrealdb_rocksdb_build_profile": "$ROCKS_BUILD_PROFILE",
  "total_work_semantics": "ops is total logical work across all clients; it is not multiplied by client count",
  "writer_semantics": "write-burst partitions whole transactions only; clients never receive a benchmark-manufactured partial transaction",
  "surrealdb_handle": "native cloned Surreal client handles over embedded SurrealKV",
  "surrealdb_rocksdb_handle": "native cloned Surreal client handles over isolated embedded RocksDB",
  "turso_handle": "independent Database::connect() connections with native 60s busy timeout",
  "sqlite_handle": "independent WAL connections with native 60s busy timeout",
  "surreal_conflict_policy": "retry only typed surrealdb::types::QueryError::TransactionConflict, bounded at 10000; retry time is included in operation/transaction latency",
  "targeted_sweeps": ["relaxed durability", "large payload", "64-record high-contention read-heavy hot set", "transaction-size extremes 1 and 1000"]
}
JSON

clear_failure() {
  local case_id=$1 failures="$RUN_DIR/failures.ndjson"
  [[ -f "$failures" ]] || return 0
  local tmp="${failures}.tmp"
  jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' "$failures" > "$tmp"
  mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}

INDEX=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1))
  IFS='|' read -r scenario engine dur workload clients records ops payload txn trial <<< "$job"
  case_id="t${trial}-${scenario}-${engine}-${dur}-${workload}-c${clients}-n${records}-o${ops}-p${payload}-tx${txn}"
  out="$RUN_DIR/cases/$case_id.json"
  if [[ -s "$out" ]]; then clear_failure "$case_id"; continue; fi
  err="$RUN_DIR/stderr/$case_id.log"
  echo "[$INDEX/$TOTAL] $case_id" >&2
  CASE_BIN="$BIN"
  [[ "$engine" == surrealdb-rocksdb ]] && CASE_BIN="$ROCKS_BIN"
  "$CASE_BIN" --engine "$engine" --durability "$dur" --workload "$workload" \
    --clients "$clients" --records "$records" --ops "$ops" \
    --payload-bytes "$payload" --txn-size "$txn" --trial "$trial" --seed 1592606758 \
    --scenario "$scenario" --warmup-reads 5000 --root "$DATA_DIR" --output "$out" 2>"$err"
  rc=$?
  if (( rc != 0 )) || [[ ! -s "$out" ]]; then
    rm -f "$out"; clear_failure "$case_id"
    jq -cn --arg case_id "$case_id" --arg stderr "$err" --argjson rc "$rc" \
      '{case_id:$case_id,returncode:$rc,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
  else
    clear_failure "$case_id"
    [[ -s "$err" ]] || rm -f "$err"
  fi
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" \
  --expect-trials "$TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"
summary_rc=$?
if [[ -s "$RUN_DIR/failures.ndjson" ]]; then FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson"); else FAILURES=0; fi
printf 'run=%s total=%s completed=%s failures=%s summary_rc=%s results=%s\n' \
  "$RUN_ID" "$TOTAL" "$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)" "$FAILURES" "$summary_rc" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 || summary_rc != 0 ? 1 : 0 ))
