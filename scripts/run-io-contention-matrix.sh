#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
BASELINE_DIR=${2:-${BASELINE_DIR:-}}
if [[ -z "$BASELINE_DIR" ]]; then
  echo "usage: $0 [smoke|quick|full] BASELINE_RUN_DIR" >&2
  echo "BASELINE_RUN_DIR must contain direct-randrw70-4k-q1.json from run-io-baseline.sh" >&2
  exit 2
fi
BASELINE_JSON="$BASELINE_DIR/direct-randrw70-4k-q1.json"
[[ -s "$BASELINE_JSON" ]] || { echo "missing $BASELINE_JSON" >&2; exit 2; }

case "$PROFILE" in
  smoke)
    TRIALS=1; RECORDS=5000; OPS=2000; PRESSURE_SIZE=256M; MIN_FREE_GIB=2; LEVELS=(0 30 60)
    ;;
  quick)
    TRIALS=3; RECORDS=100000; OPS=50000; PRESSURE_SIZE=2G; MIN_FREE_GIB=10; LEVELS=(0 10 30 60)
    ;;
  full)
    TRIALS=7; RECORDS=1000000; OPS=250000; PRESSURE_SIZE=8G; MIN_FREE_GIB=30; LEVELS=(0 10 30 60 90)
    ;;
  *) echo "usage: $0 [smoke|quick|full] BASELINE_RUN_DIR" >&2; exit 2 ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
CPUS=$(getconf _NPROCESSORS_ONLN)
LOAD1=$(awk '{print $1}' /proc/loadavg)
IO_PSI10=$(awk '/^full / {for(i=1;i<=NF;i++) if($i ~ /^avg10=/){split($i,a,"="); print a[2]}}' /proc/pressure/io)
if [[ "${ALLOW_BUSY:-0}" != 1 ]]; then
  if ! awk -v l="$LOAD1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 0.5) }'; then
    echo "refusing I/O-contention benchmark on busy host: load1=$LOAD1 visible_cpus=$CPUS" >&2
    exit 75
  fi
  if ! awk -v p="${IO_PSI10:-0}" 'BEGIN { exit !(p <= 5.0) }'; then
    echo "refusing I/O-contention benchmark under existing I/O pressure: io PSI full avg10=${IO_PSI10}%" >&2
    exit 75
  fi
fi

BASE_IOPS=$(jq -r '(.jobs[0].read.iops // 0) + (.jobs[0].write.iops // 0)' "$BASELINE_JSON")
if ! awk -v x="$BASE_IOPS" 'BEGIN { exit !(x > 0) }'; then
  echo "invalid baseline aggregate IOPS: $BASE_IOPS" >&2
  exit 2
fi

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-io-contention-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,pressure} "$DATA_DIR"
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
if (( free_bytes < min_free_bytes )); then
  echo "refusing I/O-contention run: free=$free_bytes required=$min_free_bytes" >&2
  exit 75
fi
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
cp "$BASELINE_JSON" "$RUN_DIR/baseline-randrw.json"

TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}"
BIN="$TARGET_DIR/release/kvbench"
CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.98.1.sh" build --release --features kv-all --bin kvbench

PRESSURE_FILE="$DATA_DIR/fio-pressure.bin"
fio --name=prepare-pressure --filename="$PRESSURE_FILE" --size="$PRESSURE_SIZE" \
  --rw=write --bs=1M --ioengine=psync --direct=1 --fsync_on_close=1 \
  --output-format=json > "$RUN_DIR/pressure-prepare.json"

ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
WORKLOADS=(point-read write-burst churn)
primary_durability() {
  case "$1" in
    paritydb-hash|paritydb-btree) echo relaxed ;;
    *) echo sync ;;
  esac
}

JOBS=()
for trial in $(seq 1 "$TRIALS"); do
  for pct in "${LEVELS[@]}"; do
    for workload in "${WORKLOADS[@]}"; do
      for engine in "${ENGINES[@]}"; do
        dur=$(primary_durability "$engine")
        JOBS+=("$trial|$pct|$workload|$engine|$dur")
      done
    done
  done
done
mapfile -t ORDERED < <(printf '%s\n' "${JOBS[@]}" | shuf)

pressure_pid=""
stop_pressure() {
  if [[ -n "$pressure_pid" ]] && kill -0 "$pressure_pid" 2>/dev/null; then
    kill -INT "$pressure_pid" 2>/dev/null || true
    wait "$pressure_pid" 2>/dev/null || true
  fi
  pressure_pid=""
}
cleanup() {
  stop_pressure
  rm -f -- "$PRESSURE_FILE"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

FAILURES=0
INDEX=0
TOTAL=${#ORDERED[@]}
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1))
  IFS='|' read -r trial pct workload engine dur <<< "$job"
  case_id="t${trial}-io${pct}pct-${engine}-${dur}-${workload}"
  out="$RUN_DIR/cases/$case_id.json"
  [[ -s "$out" ]] && continue
  echo "[$INDEX/$TOTAL] $case_id" >&2

  pressure_pid=""
  pressure_out="$RUN_DIR/pressure/$case_id.json"
  if (( pct > 0 )); then
    target_total=$(awk -v b="$BASE_IOPS" -v p="$pct" 'BEGIN { printf "%.0f", b*p/100.0 }')
    (( target_total < 1 )) && target_total=1
    read_iops=$(( target_total * 70 / 100 ))
    write_iops=$(( target_total - read_iops ))
    (( read_iops < 1 )) && read_iops=1
    (( write_iops < 1 )) && write_iops=1
    fio --name=pressure --filename="$PRESSURE_FILE" --size="$PRESSURE_SIZE" \
      --rw=randrw --rwmixread=70 --bs=4k --ioengine=libaio --iodepth=1 --direct=1 \
      --rate_iops="${read_iops},${write_iops}" --time_based=1 --runtime=86400 --ramp_time=1 \
      --randrepeat=1 --group_reporting=1 --output-format=json --output="$pressure_out" &
    pressure_pid=$!
    sleep 2
    if ! kill -0 "$pressure_pid" 2>/dev/null; then
      echo "fio pressure process exited before benchmark" >&2
      wait "$pressure_pid" || true
      pressure_pid=""
      FAILURES=$((FAILURES + 1))
      continue
    fi
  else
    jq -cn --argjson baseline_iops "$BASE_IOPS" '{pressure_percent:0, baseline_iops:$baseline_iops}' > "$pressure_out"
  fi

  err="$RUN_DIR/stderr/$case_id.log"
  "$BIN" --engine "$engine" --durability "$dur" --workload "$workload" \
    --records "$RECORDS" --ops "$OPS" --value-bytes 256 --txn-size 100 \
    --trial "$trial" --seed 1592606758 --scenario "io-pressure-${pct}pct" \
    --root "$DATA_DIR" --output "$out" 2>"$err"
  rc=$?
  stop_pressure

  if (( rc != 0 )); then
    FAILURES=$((FAILURES + 1))
    rm -f "$out"
    jq -cn --arg case_id "$case_id" --arg stderr "$err" --argjson rc "$rc" \
      '{case_id:$case_id, returncode:$rc, stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
  elif [[ ! -s "$err" ]]; then
    rm -f "$err"
  fi
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" \
  --expect-trials "$TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md" || true
printf 'run=%s baseline_iops=%s total=%s failures=%s results=%s\n' \
  "$RUN_ID" "$BASE_IOPS" "$TOTAL" "$FAILURES" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 ? 1 : 0 ))
