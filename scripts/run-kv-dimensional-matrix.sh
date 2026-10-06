#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke)
    TRIALS=1; RECORDS=2000; OPS=1000
    KEY_SIZES=(8 64)
    VALUE_SIZES=(256 4096)
    MISS_RATIOS=(0 10 100)
    SETTLE_WINDOWS=(0 500)
    ;;
  quick)
    TRIALS=3; RECORDS=100000; OPS=50000
    KEY_SIZES=(8 16 64 256)
    VALUE_SIZES=(256 4096)
    MISS_RATIOS=(0 1 10 50 100)
    SETTLE_WINDOWS=(0 1000 5000)
    ;;
  full)
    TRIALS=7; RECORDS=1000000; OPS=250000
    KEY_SIZES=(8 16 64 256)
    VALUE_SIZES=(256 4096 16384)
    MISS_RATIOS=(0 1 10 50 100)
    SETTLE_WINDOWS=(0 1000 10000 30000)
    ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
# shellcheck source=kv-matrix-policy.sh
source "$ROOT/scripts/kv-matrix-policy.sh"
CPUS=$(getconf _NPROCESSORS_ONLN)
LOAD1=$(awk '{print $1}' /proc/loadavg)
if [[ "${ALLOW_BUSY:-0}" != 1 ]] && ! awk -v l="$LOAD1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 1.5) }'; then
  echo "refusing dimensional benchmark on busy host: load1=$LOAD1 visible_cpus=$CPUS" >&2
  exit 75
fi

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-kv-dimensional-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,device} "$DATA_DIR"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"

TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}"
BIN="$TARGET_DIR/release/kvbench"
CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --features kv-all --bin kvbench

ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
JOBS=()

primary_durability() {
  case "$1" in
    paritydb-hash|paritydb-btree) echo relaxed ;;
    *) echo sync ;;
  esac
}

valid() {
  local engine=$1 dur=$2 workload=$3 key_shape=$4
  [[ "$engine" == lkv && "$dur" == relaxed ]] && return 1
  [[ "$engine" == manifold && "$dur" == relaxed ]] && return 1
  [[ "$engine" == jammdb && "$dur" == relaxed ]] && return 1
  [[ "$engine" == lsmdb && "$dur" == relaxed ]] && return 1
  [[ "$dur" == sync && ( "$engine" == paritydb-hash || "$engine" == paritydb-btree ) ]] && return 1
  [[ "$engine" == lkv && "$workload" == range-scan ]] && return 1
  [[ "$engine" == paritydb-hash && "$workload" == range-scan ]] && return 1
  [[ "$key_shape" == hashed && "$workload" == range-scan ]] && return 1
  return 0
}

add_job() {
  local scenario=$1 engine=$2 dur=$3 workload=$4 records=$5 ops=$6 value=$7 value_pattern=$8
  local key_bytes=$9 key_shape=${10} access=${11} miss=${12} write_pattern=${13}
  local txn=${14} scan=${15} settle=${16} trial=${17}
  valid "$engine" "$dur" "$workload" "$key_shape" || return 0
  ops=$(kv_effective_ops "$engine" "$workload" "$ops" "$records") || return 2
  JOBS+=("$scenario|$engine|$dur|$workload|$records|$ops|$value|$value_pattern|$key_bytes|$key_shape|$access|$miss|$write_pattern|$txn|$scan|$settle|$trial")
}

for trial in $(seq 1 "$TRIALS"); do
  # Read locality: uniform and two increasingly skewed hot sets.
  for access in uniform hot80 hot95; do
    for workload in point-read read-heavy; do
      for engine in "${ENGINES[@]}"; do
        add_job "access-$access" "$engine" "$(primary_durability "$engine")" "$workload" "$RECORDS" "$OPS" 256 pseudo-random 8 sequential "$access" 0 append 100 100 0 "$trial"
      done
    done
  done

  # Negative lookup dependence.
  for miss in "${MISS_RATIOS[@]}"; do
    for engine in "${ENGINES[@]}"; do
      add_job "miss-p$miss" "$engine" "$(primary_durability "$engine")" point-read "$RECORDS" "$OPS" 256 pseudo-random 8 sequential uniform "$miss" append 100 100 0 "$trial"
    done
  done

  # Key length and shape. Point reads cover hashed keys; ordered shapes also get scan cases.
  for key_bytes in "${KEY_SIZES[@]}"; do
    for key_shape in sequential shared-prefix hashed; do
      for engine in "${ENGINES[@]}"; do
        add_job "key-k${key_bytes}-${key_shape}" "$engine" "$(primary_durability "$engine")" point-read "$RECORDS" "$OPS" 256 pseudo-random "$key_bytes" "$key_shape" uniform 0 append 100 100 0 "$trial"
      done
    done
    for key_shape in sequential shared-prefix; do
      for engine in "${ENGINES[@]}"; do
        add_job "keyscan-k${key_bytes}-${key_shape}" "$engine" "$(primary_durability "$engine")" range-scan "$RECORDS" "$((OPS / 10 + 1))" 256 pseudo-random "$key_bytes" "$key_shape" uniform 0 append 100 100 0 "$trial"
      done
    done
  done

  # Value entropy/compressibility at small and large value sizes.
  for value in "${VALUE_SIZES[@]}"; do
    for value_pattern in pseudo-random zeros repeated; do
      for workload in point-read write-burst; do
        for engine in "${ENGINES[@]}"; do
          add_job "value-${value}-${value_pattern}" "$engine" "$(primary_durability "$engine")" "$workload" "$RECORDS" "$OPS" "$value" "$value_pattern" 8 sequential uniform 0 append 100 100 0 "$trial"
        done
      done
    done
  done

  # Append vs in-place updates.
  for write_pattern in append update-uniform update-hot; do
    for engine in "${ENGINES[@]}"; do
      add_job "write-${write_pattern}" "$engine" "$(primary_durability "$engine")" write-burst "$RECORDS" "$OPS" 256 pseudo-random 8 sequential hot80 0 "$write_pattern" 100 100 0 "$trial"
    done
  done

  # Tombstones and deferred/background work. Foreground metrics end before settle.
  delete_ops=$(( OPS < RECORDS ? OPS : RECORDS ))
  for settle in "${SETTLE_WINDOWS[@]}"; do
    for engine in "${ENGINES[@]}"; do
      add_job "delete-settle-${settle}ms" "$engine" "$(primary_durability "$engine")" delete-burst "$RECORDS" "$delete_ops" 256 pseudo-random 8 sequential uniform 0 append 100 100 "$settle" "$trial"
      add_job "write-settle-${settle}ms" "$engine" "$(primary_durability "$engine")" write-burst "$RECORDS" "$OPS" 256 pseudo-random 8 sequential uniform 0 append 100 100 "$settle" "$trial"
    done
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
  IFS='|' read -r scenario engine dur workload records ops value value_pattern key_bytes key_shape access miss write_pattern txn scan settle trial <<< "$job"
  case_id="t${trial}-${scenario}-${engine}-${dur}-${workload}-n${records}-o${ops}-v${value}-vp${value_pattern}-k${key_bytes}-ks${key_shape}-a${access}-m${miss}-w${write_pattern}-tx${txn}-s${scan}-st${settle}"
  case_out="$RUN_DIR/cases/$case_id.json"
  [[ -s "$case_out" ]] && continue
  echo "[$INDEX/$TOTAL] $case_id" >&2

  before=""
  [[ -n "$DEV" && -r "/sys/class/block/$DEV/stat" ]] && before=$(cat "/sys/class/block/$DEV/stat")
  err="$RUN_DIR/stderr/$case_id.log"

  "$BIN" --engine "$engine" --durability "$dur" --workload "$workload" \
    --records "$records" --ops "$ops" --value-bytes "$value" --value-pattern "$value_pattern" \
    --key-bytes "$key_bytes" --key-shape "$key_shape" --access-pattern "$access" \
    --miss-percent "$miss" --write-pattern "$write_pattern" --txn-size "$txn" \
    --scan-len "$scan" --settle-ms "$settle" --trial "$trial" --seed 1592606758 \
    --scenario "$scenario" --root "$DATA_DIR" --output "$case_out" 2>"$err"
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
