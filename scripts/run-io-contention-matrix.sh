#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
for tool in fio jq findmnt sha256sum uv awk realpath shuf; do
  command -v "$tool" >/dev/null 2>&1 || { echo "required tool not found: $tool" >&2; exit 127; }
done
BASELINE_DIR=${2:-${BASELINE_DIR:-}}
if [[ -z "$BASELINE_DIR" ]]; then
  echo "usage: $0 [smoke|quick|full] BASELINE_RUN_DIR" >&2
  echo "BASELINE_RUN_DIR must be a completed run from run-io-baseline.sh" >&2
  exit 2
fi
BASELINE_DIR=$(realpath "$BASELINE_DIR") || { echo "invalid baseline directory: $BASELINE_DIR" >&2; exit 2; }
BASELINE_JSON="$BASELINE_DIR/direct-randrw70-4k-q1.json"
CALIBRATION_JSON="$BASELINE_DIR/calibration.json"
[[ -s "$BASELINE_JSON" ]] || { echo "missing $BASELINE_JSON" >&2; exit 2; }
[[ -s "$CALIBRATION_JSON" ]] || { echo "missing completed calibration manifest: $CALIBRATION_JSON" >&2; exit 2; }
if ! jq -e '.format_version == 1 and .lane == "io-calibration" and .complete == true' "$CALIBRATION_JSON" >/dev/null; then
  echo "invalid or incomplete calibration manifest: $CALIBRATION_JSON" >&2
  exit 2
fi

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

BASELINE_SHA=$(sha256sum "$BASELINE_JSON" | awk '{print $1}')
MANIFEST_SHA=$(jq -r '.randrw70_4k_q1.sha256 // empty' "$CALIBRATION_JSON")
if [[ -z "$MANIFEST_SHA" || "$BASELINE_SHA" != "$MANIFEST_SHA" ]]; then
  echo "calibration baseline hash mismatch: manifest=$MANIFEST_SHA actual=$BASELINE_SHA" >&2
  exit 2
fi
BASE_IOPS=$(jq -r '(.jobs[0].read.iops // 0) + (.jobs[0].write.iops // 0)' "$BASELINE_JSON")
MANIFEST_IOPS=$(jq -r '.randrw70_4k_q1.iops // 0' "$CALIBRATION_JSON")
if ! awk -v a="$BASE_IOPS" -v b="$MANIFEST_IOPS" 'BEGIN { d=a-b; if(d<0)d=-d; scale=(a>1?a:1); exit !(a>0 && b>0 && d/scale < 1e-9) }'; then
  echo "calibration IOPS mismatch: manifest=$MANIFEST_IOPS baseline_json=$BASE_IOPS" >&2
  exit 2
fi

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-io-contention-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,pressure,pressure-meta,calibration} "$DATA_DIR"
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
if (( free_bytes < min_free_bytes )); then
  echo "refusing I/O-contention run: free=$free_bytes required=$min_free_bytes" >&2
  exit 75
fi

CURRENT_HOST=$(hostname)
CURRENT_MACHINE_ID=$(cat /etc/machine-id 2>/dev/null || printf unknown)
CURRENT_MOUNT_SOURCE=$(findmnt -T "$DATA_DIR" -n -o SOURCE)
CURRENT_FSTYPE=$(findmnt -T "$DATA_DIR" -n -o FSTYPE)
CURRENT_STAT_DEVICE=$(stat -c '%d' "$DATA_DIR")
CAL_HOST=$(jq -r '.hostname // empty' "$CALIBRATION_JSON")
CAL_MACHINE_ID=$(jq -r '.machine_id // empty' "$CALIBRATION_JSON")
CAL_MOUNT_SOURCE=$(jq -r '.storage.mount_source // empty' "$CALIBRATION_JSON")
CAL_FSTYPE=$(jq -r '.storage.filesystem // empty' "$CALIBRATION_JSON")
CAL_STAT_DEVICE=$(jq -r '.storage.stat_device // empty' "$CALIBRATION_JSON")
if [[ "$CURRENT_HOST" != "$CAL_HOST" || "$CURRENT_MACHINE_ID" != "$CAL_MACHINE_ID" ]]; then
  echo "calibration host mismatch: calibration=$CAL_HOST/$CAL_MACHINE_ID current=$CURRENT_HOST/$CURRENT_MACHINE_ID" >&2
  exit 2
fi
if [[ "$CURRENT_MOUNT_SOURCE" != "$CAL_MOUNT_SOURCE" || "$CURRENT_FSTYPE" != "$CAL_FSTYPE" || "$CURRENT_STAT_DEVICE" != "$CAL_STAT_DEVICE" ]]; then
  echo "calibration filesystem mismatch:" >&2
  echo "  calibration source=$CAL_MOUNT_SOURCE fs=$CAL_FSTYPE dev=$CAL_STAT_DEVICE" >&2
  echo "  current     source=$CURRENT_MOUNT_SOURCE fs=$CURRENT_FSTYPE dev=$CURRENT_STAT_DEVICE" >&2
  exit 2
fi

"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
cp "$BASELINE_JSON" "$RUN_DIR/calibration/direct-randrw70-4k-q1.json"
cp "$CALIBRATION_JSON" "$RUN_DIR/calibration/calibration.json"
[[ -s "$BASELINE_DIR/io-summary.json" ]] && cp "$BASELINE_DIR/io-summary.json" "$RUN_DIR/calibration/io-summary.json"
[[ -s "$BASELINE_DIR/io-summary.md" ]] && cp "$BASELINE_DIR/io-summary.md" "$RUN_DIR/calibration/io-summary.md"

TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}"
BIN="$TARGET_DIR/release/kvbench"
CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features kv-all --bin kvbench || exit $?
BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}')

PRESSURE_FILE="$DATA_DIR/fio-pressure.bin"
fio --name=prepare-pressure --filename="$PRESSURE_FILE" --size="$PRESSURE_SIZE" \
  --rw=write --bs=1M --ioengine=psync --direct=1 --fsync_on_close=1 \
  --output-format=json > "$RUN_DIR/pressure-prepare.json" || exit $?

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
printf '%s\n' "${ORDERED[@]}" > "$RUN_DIR/jobs.txt"
TOTAL=${#ORDERED[@]}

jq -n \
  --arg profile "$PROFILE" \
  --arg baseline_run_id "$(jq -r '.run_id' "$CALIBRATION_JSON")" \
  --arg baseline_sha256 "$BASELINE_SHA" \
  --arg benchmark_binary_sha256 "$BIN_SHA" \
  --argjson baseline_iops "$BASE_IOPS" \
  --argjson trials "$TRIALS" \
  --argjson case_count "$TOTAL" \
  '{lane:"io-pressure",profile:$profile,trials:$trials,case_count:$case_count,baseline_run_id:$baseline_run_id,baseline_randrw70_4k_q1_sha256:$baseline_sha256,baseline_randrw70_4k_q1_iops:$baseline_iops,benchmark_binary_sha256:$benchmark_binary_sha256,pressure:"independent same-filesystem direct 4KiB QD1 70/30 fio rate-limited to calibrated baseline fractions; delivered IOPS retained per case"}' \
  > "$RUN_DIR/support.json"

pressure_pid=""
stop_pressure() {
  if [[ -n "$pressure_pid" ]] && kill -0 "$pressure_pid" 2>/dev/null; then
    kill -INT "$pressure_pid" 2>/dev/null || true
    wait "$pressure_pid" 2>/dev/null || true
  elif [[ -n "$pressure_pid" ]]; then
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

clear_failure() {
  local case_id=$1
  local failures="$RUN_DIR/failures.ndjson"
  [[ -f "$failures" ]] || return 0
  local tmp="${failures}.tmp"
  jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' "$failures" > "$tmp"
  mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}
record_failure() {
  local case_id=$1 rc=$2 stderr=$3 reason=$4
  clear_failure "$case_id"
  jq -cn --arg case_id "$case_id" --arg stderr "$stderr" --arg reason "$reason" --argjson rc "$rc" \
    '{case_id:$case_id,returncode:$rc,stderr:$stderr,reason:$reason}' >> "$RUN_DIR/failures.ndjson"
}

INDEX=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1))
  IFS='|' read -r trial pct workload engine dur <<< "$job"
  case_id="t${trial}-io${pct}pct-${engine}-${dur}-${workload}"
  out="$RUN_DIR/cases/$case_id.json"
  pressure_meta="$RUN_DIR/pressure-meta/$case_id.json"
  if [[ -s "$out" && -s "$pressure_meta" ]]; then
    clear_failure "$case_id"
    continue
  fi
  rm -f "$out" "$pressure_meta"
  echo "[$INDEX/$TOTAL] $case_id" >&2

  pressure_pid=""
  pressure_out="$RUN_DIR/pressure/$case_id.json"
  target_total=0
  read_iops=0
  write_iops=0
  if (( pct > 0 )); then
    target_total=$(awk -v b="$BASE_IOPS" -v p="$pct" 'BEGIN { printf "%.0f", b*p/100.0 }')
    (( target_total < 1 )) && target_total=1
    read_iops=$(( target_total * 70 / 100 ))
    write_iops=$(( target_total - read_iops ))
    (( read_iops < 1 )) && read_iops=1
    (( write_iops < 1 )) && write_iops=1
    target_total=$((read_iops + write_iops))
    rm -f "$pressure_out"
    fio --name=pressure --filename="$PRESSURE_FILE" --size="$PRESSURE_SIZE" \
      --rw=randrw --rwmixread=70 --bs=4k --ioengine=libaio --iodepth=1 --direct=1 \
      --rate_iops="${read_iops},${write_iops}" --time_based=1 --runtime=86400 --ramp_time=1 \
      --randrepeat=1 --group_reporting=1 --output-format=json --output="$pressure_out" &
    pressure_pid=$!
    sleep 2
    if ! kill -0 "$pressure_pid" 2>/dev/null; then
      wait "$pressure_pid" 2>/dev/null || true
      pressure_pid=""
      err="$RUN_DIR/stderr/$case_id.log"
      printf 'fio pressure process exited before benchmark\n' > "$err"
      record_failure "$case_id" 70 "$err" "pressure-exited-before-benchmark"
      continue
    fi
  else
    # Give the 0% control the same pre-case settling interval as pressured cases.
    sleep 2
  fi

  err="$RUN_DIR/stderr/$case_id.log"
  "$BIN" --engine "$engine" --durability "$dur" --workload "$workload" \
    --records "$RECORDS" --ops "$OPS" --value-bytes 256 --txn-size 100 \
    --trial "$trial" --seed 1592606758 --scenario "io-pressure-${pct}pct" \
    --root "$DATA_DIR" --output "$out" 2>"$err"
  db_rc=$?

  pressure_alive=1
  if (( pct > 0 )) && ! kill -0 "$pressure_pid" 2>/dev/null; then
    pressure_alive=0
  fi
  stop_pressure

  delivered_read=0
  delivered_write=0
  delivered_total=0
  pressure_valid=1
  if (( pct > 0 )); then
    if (( pressure_alive == 0 )) || [[ ! -s "$pressure_out" ]] || ! jq -e '.jobs | length == 1' "$pressure_out" >/dev/null 2>&1; then
      pressure_valid=0
    else
      delivered_read=$(jq -r '.jobs[0].read.iops // 0' "$pressure_out")
      delivered_write=$(jq -r '.jobs[0].write.iops // 0' "$pressure_out")
      delivered_total=$(awk -v r="$delivered_read" -v w="$delivered_write" 'BEGIN { printf "%.9f", r+w }')
      if ! awk -v x="$delivered_total" 'BEGIN { exit !(x > 0) }'; then
        pressure_valid=0
      fi
    fi
  fi

  delivered_ratio=null
  if (( target_total > 0 )) && (( pressure_valid == 1 )); then
    delivered_ratio=$(awk -v d="$delivered_total" -v t="$target_total" 'BEGIN { printf "%.9f", d/t }')
  fi
  jq -n \
    --arg case_id "$case_id" \
    --argjson pressure_percent "$pct" \
    --argjson baseline_iops "$BASE_IOPS" \
    --argjson target_iops "$target_total" \
    --argjson target_read_iops "$read_iops" \
    --argjson target_write_iops "$write_iops" \
    --argjson delivered_read_iops "$delivered_read" \
    --argjson delivered_write_iops "$delivered_write" \
    --argjson delivered_iops "$delivered_total" \
    --argjson delivered_vs_target "$delivered_ratio" \
    --argjson pressure_worker_valid "$pressure_valid" \
    '{case_id:$case_id,pressure_percent:$pressure_percent,baseline_iops:$baseline_iops,target_iops:$target_iops,target_read_iops:$target_read_iops,target_write_iops:$target_write_iops,delivered_read_iops:$delivered_read_iops,delivered_write_iops:$delivered_write_iops,delivered_iops:$delivered_iops,delivered_vs_target:$delivered_vs_target,pressure_worker_valid:($pressure_worker_valid == 1)}' \
    > "$pressure_meta"

  if (( db_rc != 0 )); then
    rm -f "$out"
    record_failure "$case_id" "$db_rc" "$err" "benchmark-failed"
  elif (( pressure_valid == 0 )); then
    printf 'fio pressure worker did not remain valid for the full benchmark interval\n' >> "$err"
    rm -f "$out"
    record_failure "$case_id" 70 "$err" "pressure-invalid-during-benchmark"
  else
    clear_failure "$case_id"
    [[ -s "$err" ]] || rm -f "$err"
  fi
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" \
  --expect-trials "$TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md" || true
if [[ -s "$RUN_DIR/results.ndjson" ]]; then
  uv run --script "$ROOT/scripts/summarize-io-pressure.py" "$RUN_DIR" \
    --json-out "$RUN_DIR/io-pressure-summary.json" --markdown-out "$RUN_DIR/io-pressure-summary.md" || true
fi

FAILURES=0
[[ -s "$RUN_DIR/failures.ndjson" ]] && FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson")
SUCCESSFUL=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
printf 'run=%s baseline_iops=%s total=%s successful=%s failures=%s results=%s\n' \
  "$RUN_ID" "$BASE_IOPS" "$TOTAL" "$SUCCESSFUL" "$FAILURES" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 ? 1 : 0 ))
