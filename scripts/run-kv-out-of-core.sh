#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke) TRIALS=1; RATIOS=(25); OPS=2000 ;;
  quick) TRIALS=3; RATIOS=(50 100); OPS=50000 ;;
  full)  TRIALS=5; RATIOS=(50 100 200); OPS=250000 ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
VALUE_BYTES=4096
KEY_BYTES=16
MEM_KIB=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)
[[ "$MEM_KIB" =~ ^[0-9]+$ ]] || { echo "cannot read MemTotal" >&2; exit 2; }
CPUS=$(getconf _NPROCESSORS_ONLN)
LOAD1=$(awk '{print $1}' /proc/loadavg)
if [[ "${ALLOW_BUSY:-0}" != 1 ]] && ! awk -v l="$LOAD1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 1.5) }'; then
  echo "refusing out-of-core benchmark on busy host: load1=$LOAD1 visible_cpus=$CPUS" >&2
  exit 75
fi

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-kv-out-of-core-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr} "$DATA_DIR"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"

TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}"
BIN="$TARGET_DIR/release/kvbench"
CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.98.1.sh" build --release --features kv-all --bin kvbench

ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv rocksdb mdbx persy roughdb jammdb lsmdb)
WORKLOADS=(point-read read-heavy churn)
JOBS=()

for ratio in "${RATIOS[@]}"; do
  target_bytes=$(( MEM_KIB * 1024 * ratio / 100 ))
  records=$(( target_bytes / (VALUE_BYTES + KEY_BYTES) ))
  (( records < 1000 )) && records=1000

  # One case exists at a time, but allow substantial temporary/compaction amplification.
  need_bytes=$(( target_bytes * 4 + 2 * 1024 * 1024 * 1024 ))
  avail_bytes=$(df -PB1 "$DATA_DIR" | awk 'NR==2 {print $4}')
  if (( avail_bytes < need_bytes )); then
    echo "insufficient free space for ratio=${ratio}%: need≈$need_bytes available=$avail_bytes" >&2
    exit 28
  fi

  for trial in $(seq 1 "$TRIALS"); do
    for workload in "${WORKLOADS[@]}"; do
      for engine in "${ENGINES[@]}"; do
        JOBS+=("$ratio|$records|$trial|$workload|$engine")
      done
    done
  done
done

mapfile -t ORDERED < <(printf '%s\n' "${JOBS[@]}" | shuf)
FAILURES=0
INDEX=0
TOTAL=${#ORDERED[@]}
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1))
  IFS='|' read -r ratio records trial workload engine <<< "$job"
  case_id="t${trial}-logical-footprint-${ratio}pct-ram-${engine}-sync-${workload}-n${records}"
  out="$RUN_DIR/cases/$case_id.json"
  [[ -s "$out" ]] && continue
  echo "[$INDEX/$TOTAL] $case_id" >&2
  err="$RUN_DIR/stderr/$case_id.log"

  "$BIN" --engine "$engine" --durability sync --workload "$workload" \
    --records "$records" --ops "$OPS" --key-bytes "$KEY_BYTES" --key-shape sequential \
    --value-bytes "$VALUE_BYTES" --value-pattern pseudo-random --txn-size 100 \
    --access-pattern uniform --trial "$trial" --seed 1592606758 \
    --scenario "logical-footprint-${ratio}pct-ram" --root "$DATA_DIR" --output "$out" 2>"$err"
  rc=$?

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
printf 'run=%s mem_kib=%s total=%s failures=%s results=%s\n' \
  "$RUN_ID" "$MEM_KIB" "$TOTAL" "$FAILURES" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 ? 1 : 0 ))
