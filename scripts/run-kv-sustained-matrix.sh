#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke)
    TRIALS=1
    CORE_RECORDS=5000; CORE_OPS=10000; CORE_WINDOW=1000; CORE_VALUE=256
    STRESS_RECORDS=5000; STRESS_OPS=10000; STRESS_WINDOW=1000
    RELAXED_RECORDS=5000; RELAXED_OPS=10000; RELAXED_WINDOW=1000
    TX_RECORDS=5000; TX_OPS=5000; TX_WINDOW=1000
    SETTLE_MS=500; SETTLE_SAMPLE_MS=100; MIN_FREE_GIB=5
    CORE_PATTERNS=(append churn)
    ;;
  quick)
    TRIALS=3
    CORE_RECORDS=100000; CORE_OPS=250000; CORE_WINDOW=10000; CORE_VALUE=256
    STRESS_RECORDS=50000; STRESS_OPS=125000; STRESS_WINDOW=5000
    RELAXED_RECORDS=100000; RELAXED_OPS=125000; RELAXED_WINDOW=5000
    TX_RECORDS=50000; TX_OPS=20000; TX_WINDOW=2000
    SETTLE_MS=5000; SETTLE_SAMPLE_MS=250; MIN_FREE_GIB=20
    CORE_PATTERNS=(append update-uniform update-hot95 churn)
    ;;
  full)
    TRIALS=5
    CORE_RECORDS=1000000; CORE_OPS=2000000; CORE_WINDOW=50000; CORE_VALUE=256
    STRESS_RECORDS=250000; STRESS_OPS=1000000; STRESS_WINDOW=25000
    RELAXED_RECORDS=500000; RELAXED_OPS=1000000; RELAXED_WINDOW=25000
    TX_RECORDS=100000; TX_OPS=100000; TX_WINDOW=5000
    SETTLE_MS=30000; SETTLE_SAMPLE_MS=500; MIN_FREE_GIB=50
    CORE_PATTERNS=(append update-uniform update-hot95 churn)
    ;;
  *)
    echo "usage: $0 [smoke|quick|full]" >&2
    exit 2
    ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
CPUS=$(getconf _NPROCESSORS_ONLN)
LOAD1=$(awk '{print $1}' /proc/loadavg)
IO_PSI10=$(awk '/^full / {for(i=1;i<=NF;i++) if($i ~ /^avg10=/){split($i,a,"="); print a[2]}}' /proc/pressure/io)
if [[ "${ALLOW_BUSY:-0}" != 1 ]]; then
  if ! awk -v l="$LOAD1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 0.5) }'; then
    echo "refusing sustained benchmark on busy host: load1=$LOAD1 cpus=$CPUS" >&2
    exit 75
  fi
  if ! awk -v p="${IO_PSI10:-0}" 'BEGIN { exit !(p <= 5.0) }'; then
    echo "refusing sustained benchmark under I/O pressure: io PSI full avg10=${IO_PSI10}%" >&2
    exit 75
  fi
fi

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-kv-sustained-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr} "$DATA_DIR"

free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
if (( free_bytes < min_free_bytes )); then
  echo "refusing sustained run: free=$free_bytes required=$min_free_bytes" >&2
  exit 75
fi

"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
else
  BIN="$TARGET_DIR/release/kvsustained"
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.98.1.sh" \
    build --release --features kv-all --bin kvsustained || exit $?
fi
BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}')

ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
STRESS_ENGINES=(fjall surrealkv sled turbokv rocksdb roughdb lsmdb)
RELAXED_ENGINES=(redb fjall surrealkv heed sled turbokv rocksdb mdbx persy roughdb)
TX_STRESS_ENGINES=(redb fjall surrealkv heed sled turbokv rocksdb mdbx persy roughdb)

primary_durability() {
  case "$1" in
    paritydb-hash|paritydb-btree) echo relaxed ;;
    *) echo sync ;;
  esac
}

JOBS=()
add_job() {
  local scenario=$1 engine=$2 durability=$3 pattern=$4 records=$5 ops=$6 window=$7 value=$8 txn=$9 trial=${10}
  JOBS+=("$scenario|$engine|$durability|$pattern|$records|$ops|$window|$value|$txn|$trial")
}

for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    dur=$(primary_durability "$engine")
    for pattern in "${CORE_PATTERNS[@]}"; do
      add_job sustained-core "$engine" "$dur" "$pattern" "$CORE_RECORDS" "$CORE_OPS" "$CORE_WINDOW" "$CORE_VALUE" 100 "$trial"
    done
  done

  for engine in "${STRESS_ENGINES[@]}"; do
    dur=$(primary_durability "$engine")
    for pattern in append churn; do
      add_job sustained-4k-stress "$engine" "$dur" "$pattern" "$STRESS_RECORDS" "$STRESS_OPS" "$STRESS_WINDOW" 4096 100 "$trial"
    done
  done

  for engine in "${RELAXED_ENGINES[@]}"; do
    for pattern in append churn; do
      add_job sustained-relaxed "$engine" relaxed "$pattern" "$RELAXED_RECORDS" "$RELAXED_OPS" "$RELAXED_WINDOW" 256 100 "$trial"
    done
  done

  for engine in "${TX_STRESS_ENGINES[@]}"; do
    for txn in 1 1000; do
      add_job sustained-txn-extremes "$engine" "$(primary_durability "$engine")" append "$TX_RECORDS" "$TX_OPS" "$TX_WINDOW" 256 "$txn" "$trial"
    done
  done
done

mapfile -t ORDERED < <(printf '%s\n' "${JOBS[@]}" | shuf)
TOTAL=${#ORDERED[@]}
printf '%s\n' "${ORDERED[@]}" > "$RUN_DIR/jobs.txt"
cat > "$RUN_DIR/support.json" <<JSON
{
  "lane": "kv-sustained",
  "profile": "$PROFILE",
  "trials": $TRIALS,
  "case_count": $TOTAL,
  "benchmark_binary_sha256": "$BIN_SHA",
  "window_method": "fixed logical-op windows; no recursive database-size scan between windows",
  "threshold_interpretation": "75/50/25% baseline ratios are reporting diagnostics, not pass/fail criteria",
  "baseline_method": "median throughput and p99 latency of the first min(3, window_count) windows",
  "primary_durability": "sync where durable-before-return is exposed; ParityDB uses its documented relaxed/background path",
  "targeted_sweeps": ["4KiB LSM-style compaction stress", "relaxed/background durability", "txn-size extremes 1 and 1000"],
  "post_workload_settle_ms": $SETTLE_MS
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
  IFS='|' read -r scenario engine dur pattern records ops window value txn trial <<< "$job"
  case_id="t${trial}-${scenario}-${engine}-${dur}-${pattern}-n${records}-o${ops}-w${window}-v${value}-tx${txn}"
  out="$RUN_DIR/cases/$case_id.json"
  if [[ -s "$out" ]]; then
    clear_failure "$case_id"
    continue
  fi
  err="$RUN_DIR/stderr/$case_id.log"
  echo "[$INDEX/$TOTAL] $case_id" >&2
  "$BIN" \
    --engine "$engine" --durability "$dur" --pattern "$pattern" \
    --records "$records" --ops "$ops" --window-ops "$window" \
    --value-bytes "$value" --value-pattern pseudo-random \
    --key-bytes 8 --key-shape sequential --txn-size "$txn" \
    --trial "$trial" --seed 1592606758 --warmup-reads 5000 \
    --settle-ms "$SETTLE_MS" --settle-sample-ms "$SETTLE_SAMPLE_MS" \
    --scenario "$scenario" --root "$DATA_DIR" --output "$out" \
    2>"$err"
  rc=$?
  if (( rc != 0 )) || [[ ! -s "$out" ]]; then
    rm -f "$out"
    clear_failure "$case_id"
    jq -cn --arg case_id "$case_id" --arg stderr "$err" --argjson rc "$rc" \
      '{case_id:$case_id,returncode:$rc,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
  else
    clear_failure "$case_id"
    [[ -s "$err" ]] || rm -f "$err"
  fi
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
uv run --script "$ROOT/scripts/summarize-sustained.py" "$RUN_DIR" \
  --expect-trials "$TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"
summary_rc=$?
if [[ -s "$RUN_DIR/failures.ndjson" ]]; then FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson"); else FAILURES=0; fi

echo "run=$RUN_ID total=$TOTAL completed=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l) failures=$FAILURES summary_rc=$summary_rc results=$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 || summary_rc != 0 ? 1 : 0 ))
