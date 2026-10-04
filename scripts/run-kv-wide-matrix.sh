#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke)
    TRIALS=1; CORE_RECORDS=1000; CORE_OPS=500
    VALUE_RECORDS=1000; VALUE_OPS=300
    SCALE_RECORDS=(100 1000 10000)
    VALUE_SIZES=(0 8 64 256 4096)
    TXN_SIZES=(1 4 16 100)
    SCAN_LENS=(1 8 100 512)
    ;;
  quick)
    TRIALS=3; CORE_RECORDS=100000; CORE_OPS=50000
    VALUE_RECORDS=20000; VALUE_OPS=10000
    SCALE_RECORDS=(1000 100000 1000000)
    VALUE_SIZES=(0 8 32 256 1024 4096 16384)
    TXN_SIZES=(1 4 16 64 256 1024)
    SCAN_LENS=(1 8 32 128 512 2048)
    ;;
  full)
    TRIALS=7; CORE_RECORDS=1000000; CORE_OPS=250000
    VALUE_RECORDS=100000; VALUE_OPS=50000
    SCALE_RECORDS=(1000 100000 1000000 5000000)
    VALUE_SIZES=(0 8 32 256 1024 4096 16384)
    TXN_SIZES=(1 4 16 64 256 1024)
    SCAN_LENS=(1 8 32 128 512 2048)
    ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
CPUS=$(getconf _NPROCESSORS_ONLN)
LOAD1=$(awk '{print $1}' /proc/loadavg)
if [[ "${ALLOW_BUSY:-0}" != 1 ]] && ! awk -v l="$LOAD1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 1.5) }'; then
  echo "refusing wide benchmark on busy host: load1=$LOAD1 visible_cpus=$CPUS" >&2
  exit 75
fi

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-kv-wide-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,device} "$DATA_DIR"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"

TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}"
BIN="$TARGET_DIR/release/kvbench"
"$ROOT/scripts/cargo-local-1.98.1.sh" build --release --features kv-all --bin kvbench

ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
DURS=(relaxed sync)
WORKLOADS=(point-read range-scan read-heavy balanced tiny-txn write-burst churn)
JOBS=()

valid() {
  local engine=$1 dur=$2 workload=$3
  [[ "$engine" == lkv && "$dur" == relaxed ]] && return 1
  [[ "$engine" == manifold && "$dur" == relaxed ]] && return 1
  [[ "$engine" == jammdb && "$dur" == relaxed ]] && return 1
  [[ "$engine" == lsmdb && "$dur" == relaxed ]] && return 1
  [[ "$dur" == sync && ( "$engine" == paritydb-hash || "$engine" == paritydb-btree ) ]] && return 1
  [[ "$engine" == lkv && "$workload" == range-scan ]] && return 1
  [[ "$engine" == paritydb-hash && "$workload" == range-scan ]] && return 1
  return 0
}

add_job() {
  local scenario=$1 engine=$2 dur=$3 workload=$4 records=$5 ops=$6 value=$7 txn=$8 scan=$9 trial=${10}
  valid "$engine" "$dur" "$workload" || return 0
  JOBS+=("$scenario|$engine|$dur|$workload|$records|$ops|$value|$txn|$scan|$trial")
}

for trial in $(seq 1 "$TRIALS"); do
  # Core: all engines, both durability lanes, all established workload shapes.
  for dur in "${DURS[@]}"; do
    for workload in "${WORKLOADS[@]}"; do
      for engine in "${ENGINES[@]}"; do
        add_job core "$engine" "$dur" "$workload" "$CORE_RECORDS" "$CORE_OPS" 256 100 100 "$trial"
      done
    done
  done

  # Dataset/working-set ladder: primary sync lane, read, mixed, and churn.
  for records in "${SCALE_RECORDS[@]}"; do
    ops=$CORE_OPS
    (( ops > records * 4 )) && ops=$((records * 4))
    (( ops < 500 )) && ops=500
    for workload in point-read read-heavy churn; do
      for engine in "${ENGINES[@]}"; do
        add_job "scale-n$records" "$engine" sync "$workload" "$records" "$ops" 256 100 100 "$trial"
      done
    done
  done

  # Value-size ladder, including metadata-dominated empty/tiny values and large values.
  for value in "${VALUE_SIZES[@]}"; do
    for workload in point-read write-burst churn; do
      for engine in "${ENGINES[@]}"; do
        add_job "value-v$value" "$engine" sync "$workload" "$VALUE_RECORDS" "$VALUE_OPS" "$value" 100 100 "$trial"
      done
    done
  done

  # Transaction-size sweep: isolates commit/barrier amortization.
  for txn in "${TXN_SIZES[@]}"; do
    for engine in "${ENGINES[@]}"; do
      add_job "txn-t$txn" "$engine" sync write-burst "$VALUE_RECORDS" "$VALUE_OPS" 256 "$txn" 100 "$trial"
    done
  done

  # Scan width sweep. Keep visited-row count bounded so 2k-row scans do not dominate runtime absurdly.
  for scan in "${SCAN_LENS[@]}"; do
    scan_ops=$(( VALUE_OPS * 100 / scan ))
    (( scan_ops < 250 )) && scan_ops=250
    (( scan_ops > VALUE_OPS )) && scan_ops=$VALUE_OPS
    for engine in "${ENGINES[@]}"; do
      add_job "scan-s$scan" "$engine" sync range-scan "$CORE_RECORDS" "$scan_ops" 256 100 "$scan" "$trial"
    done
  done

  # Deliberately awkward but plausible combinations.
  for engine in "${ENGINES[@]}"; do
    add_job awkward-sync-large-value-single "$engine" sync write-burst "$VALUE_RECORDS" "$VALUE_OPS" 4096 1 100 "$trial"
    add_job awkward-sync-tiny-value-huge-batch "$engine" sync write-burst "$VALUE_RECORDS" "$VALUE_OPS" 8 1024 100 "$trial"
    add_job awkward-sync-large-value-churn "$engine" sync churn "$VALUE_RECORDS" "$VALUE_OPS" 4096 16 100 "$trial"
    add_job awkward-relaxed-large-value-single "$engine" relaxed write-burst "$VALUE_RECORDS" "$VALUE_OPS" 4096 1 100 "$trial"
  done
done

SOURCE=$(findmnt -T "$ROOT" -n -o SOURCE 2>/dev/null || true)
DEV=""
if [[ "$SOURCE" == /dev/* ]]; then
  DEV=$(basename "$SOURCE")
  PARENT=$(lsblk -n -o PKNAME "$SOURCE" 2>/dev/null | head -n1 || true)
  [[ -n "$PARENT" ]] && DEV="$PARENT"
fi

mapfile -t ORDERED < <(printf '%s\n' "${JOBS[@]}" | shuf)
TOTAL=${#ORDERED[@]}
INDEX=0
FAILURES=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1))
  IFS='|' read -r scenario engine dur workload records ops value txn scan trial <<< "$job"
  case_id="t${trial}-${scenario}-${engine}-${dur}-${workload}-n${records}-o${ops}-v${value}-tx${txn}-s${scan}"
  case_out="$RUN_DIR/cases/$case_id.json"
  [[ -s "$case_out" ]] && continue
  echo "[$INDEX/$TOTAL] $case_id" >&2

  before=""
  [[ -n "$DEV" && -r "/sys/class/block/$DEV/stat" ]] && before=$(cat "/sys/class/block/$DEV/stat")
  err="$RUN_DIR/stderr/$case_id.log"

  "$BIN" --engine "$engine" --durability "$dur" --workload "$workload" \
    --records "$records" --ops "$ops" --value-bytes "$value" --txn-size "$txn" \
    --scan-len "$scan" --trial "$trial" --seed 1592606758 --scenario "$scenario" \
    --root "$DATA_DIR" --output "$case_out" 2>"$err"
  rc=$?

  after=""
  [[ -n "$DEV" && -r "/sys/class/block/$DEV/stat" ]] && after=$(cat "/sys/class/block/$DEV/stat")
  jq -cn --arg case_id "$case_id" --arg device "$DEV" --arg before "$before" --arg after "$after" --argjson rc "$rc" \
    '{case_id:$case_id, device:$device, block_stat_before:$before, block_stat_after:$after, returncode:$rc}' \
    > "$RUN_DIR/device/$case_id.json"

  if (( rc != 0 )); then
    FAILURES=$((FAILURES + 1))
    rm -f "$case_out"
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
printf 'run=%s total=%s failures=%s results=%s\n' "$RUN_ID" "$TOTAL" "$FAILURES" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 ? 1 : 0 ))
