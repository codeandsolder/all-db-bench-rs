#!/usr/bin/env bash
set -u -o pipefail
PROFILE=${1:-quick}
case "$PROFILE" in
  smoke)
    TRIALS=1; CORE_RECORDS=100; CORE_OPS=100
    SCALE_RECORDS=(100 1000)
    PAYLOADS=(16 128 1024)
    TXNS=(1 16 100)
    ;;
  quick)
    TRIALS=3; CORE_RECORDS=10000; CORE_OPS=10000
    SCALE_RECORDS=(1000 10000 100000)
    PAYLOADS=(16 128 512 2048 16384)
    TXNS=(1 4 16 64 256)
    ;;
  full)
    TRIALS=7; CORE_RECORDS=100000; CORE_OPS=50000
    SCALE_RECORDS=(1000 10000 100000 500000)
    PAYLOADS=(16 128 512 2048 16384)
    TXNS=(1 4 16 64 256 1024)
    ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac
ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
CPUS=$(getconf _NPROCESSORS_ONLN); LOAD1=$(awk '{print $1}' /proc/loadavg)
if [[ "${ALLOW_BUSY:-0}" != 1 ]] && ! awk -v l="$LOAD1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 1.5) }'; then
  echo "refusing wide record benchmark on busy host: load1=$LOAD1 visible_cpus=$CPUS" >&2
  exit 75
fi
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-record-wide-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"; DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr} "$DATA_DIR"
"$ROOT/scripts/ensure-sqlite-3.53.4.sh"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}"
BIN="$TARGET_DIR/release/recordbench"
"$ROOT/scripts/cargo-local-1.98.1.sh" build --release --features record --bin recordbench
ROCKS_TARGET_DIR="${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}"
"$ROOT/scripts/cargo-local-1.98.1.sh" build --release --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" --target-dir "$ROCKS_TARGET_DIR"
ROCKS_BIN="$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordbench"

JOBS=()
add() { JOBS+=("$1|$2|$3|$4|$5|$6|$7|$8|$9"); }
for trial in $(seq 1 "$TRIALS"); do
  for engine in surrealdb turso sqlite surrealdb-rocksdb; do
    for dur in relaxed sync; do
      for workload in point-read indexed-read read-heavy tiny-txn write-burst; do
        add core "$engine" "$dur" "$workload" "$CORE_RECORDS" "$CORE_OPS" 512 100 "$trial"
      done
    done
    for records in "${SCALE_RECORDS[@]}"; do
      ops=$CORE_OPS; (( ops > records * 4 )) && ops=$((records * 4)); (( ops < 100 )) && ops=100
      for workload in point-read indexed-read read-heavy; do
        add "scale-n$records" "$engine" sync "$workload" "$records" "$ops" 512 100 "$trial"
      done
    done
    for payload in "${PAYLOADS[@]}"; do
      for workload in point-read indexed-read read-heavy write-burst; do
        add "payload-p$payload" "$engine" sync "$workload" "$CORE_RECORDS" "$CORE_OPS" "$payload" 100 "$trial"
      done
    done
    for txn in "${TXNS[@]}"; do
      add "txn-t$txn" "$engine" sync write-burst "$CORE_RECORDS" "$CORE_OPS" 512 "$txn" "$trial"
    done
    add awkward-sync-large-payload-single "$engine" sync write-burst "$CORE_RECORDS" "$CORE_OPS" 16384 1 "$trial"
    add awkward-relaxed-large-payload-single "$engine" relaxed write-burst "$CORE_RECORDS" "$CORE_OPS" 16384 1 "$trial"
  done
done

mapfile -t ORDERED < <(printf '%s\n' "${JOBS[@]}" | shuf)
TOTAL=${#ORDERED[@]}; INDEX=0; FAILURES=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX+1))
  IFS='|' read -r scenario engine dur workload records ops payload txn trial <<< "$job"
  case_id="t${trial}-${scenario}-${engine}-${dur}-${workload}-n${records}-o${ops}-p${payload}-tx${txn}"
  case_out="$RUN_DIR/cases/$case_id.json"; [[ -s "$case_out" ]] && continue
  echo "[$INDEX/$TOTAL] $case_id" >&2
  err="$RUN_DIR/stderr/$case_id.log"
  CASE_BIN="$BIN"
  [[ "$engine" == surrealdb-rocksdb ]] && CASE_BIN="$ROCKS_BIN"
  "$CASE_BIN" --engine "$engine" --durability "$dur" --workload "$workload" \
    --records "$records" --ops "$ops" --payload-bytes "$payload" --txn-size "$txn" \
    --trial "$trial" --seed 1592606758 --scenario "$scenario" --root "$DATA_DIR" --output "$case_out" 2>"$err"
  rc=$?
  if (( rc != 0 )); then
    FAILURES=$((FAILURES+1)); rm -f "$case_out"
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
