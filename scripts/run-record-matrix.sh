#!/usr/bin/env bash
set -euo pipefail
PROFILE=${1:-quick}
case "$PROFILE" in
  smoke) RECORDS=50; OPS=50; TRIALS=1 ;;
  quick) RECORDS=10000; OPS=10000; TRIALS=3 ;;
  full) RECORDS=100000; OPS=50000; TRIALS=7 ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac
ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}

CPUS=$(getconf _NPROCESSORS_ONLN)
LOAD1=$(awk '{print $1}' /proc/loadavg)
if [[ "${ALLOW_BUSY:-0}" != 1 ]] && ! awk -v l="$LOAD1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 1.5) }'; then
  echo "refusing benchmark on busy host: load1=$LOAD1 visible_cpus=$CPUS (set ALLOW_BUSY=1 only for smoke/debug validation)" >&2
  exit 75
fi
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-record-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR" "$DATA_DIR"
OUT="$RUN_DIR/results.ndjson"
"$ROOT/scripts/ensure-sqlite-3.53.4.sh"
META="$RUN_DIR/host.txt"
"$ROOT/scripts/capture-host-metadata.sh" "$META" "$ROOT"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}"
BIN="$TARGET_DIR/release/recordbench"
"$ROOT/scripts/cargo-local-1.98.1.sh" build --release --features record --bin recordbench
ROCKS_TARGET_DIR="${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}"
"$ROOT/scripts/cargo-local-1.98.1.sh" build --release --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" --target-dir "$ROCKS_TARGET_DIR"
ROCKS_BIN="$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordbench"
JOBS=()
for trial in $(seq 1 "$TRIALS"); do
  for engine in surrealdb turso sqlite surrealdb-rocksdb; do
    for durability in relaxed sync; do
      for workload in point-read indexed-read read-heavy tiny-txn write-burst; do
        JOBS+=("$trial|$engine|$durability|$workload")
      done
    done
  done
done
while IFS= read -r job; do
  IFS='|' read -r trial engine durability workload <<< "$job"
  echo "trial=$trial engine=$engine durability=$durability workload=$workload" >&2
  CASE_BIN="$BIN"
  [[ "$engine" == surrealdb-rocksdb ]] && CASE_BIN="$ROCKS_BIN"
  "$CASE_BIN" --engine "$engine" --durability "$durability" --workload "$workload"     --records "$RECORDS" --ops "$OPS" --payload-bytes 512 --txn-size 100     --trial "$trial" --seed 1592606758 --scenario baseline-core --root "$DATA_DIR" --output "$OUT"
done < <(printf '%s
' "${JOBS[@]}" | shuf)
echo "$OUT"
