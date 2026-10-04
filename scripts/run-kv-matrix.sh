#!/usr/bin/env bash
set -euo pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke) RECORDS=100; OPS=100; TRIALS=1 ;;
  quick) RECORDS=100000; OPS=50000; TRIALS=3 ;;
  full)  RECORDS=1000000; OPS=250000; TRIALS=7 ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}

CPUS=$(getconf _NPROCESSORS_ONLN)
LOAD1=$(awk '{print $1}' /proc/loadavg)
if [[ "${ALLOW_BUSY:-0}" != 1 ]] && ! awk -v l="$LOAD1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 1.5) }'; then
  echo "refusing benchmark on busy host: load1=$LOAD1 visible_cpus=$CPUS (set ALLOW_BUSY=1 only for smoke/debug validation)" >&2
  exit 75
fi
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-kv-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR" "$DATA_DIR"
OUT="$RUN_DIR/results.ndjson"
META="$RUN_DIR/host.txt"
"$ROOT/scripts/capture-host-metadata.sh" "$META" "$ROOT"

ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
WORKLOADS=(point-read range-scan read-heavy balanced tiny-txn write-burst churn)
DURABILITIES=(relaxed sync)

TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}"
BIN="$TARGET_DIR/release/kvbench"
"$ROOT/scripts/cargo-local-1.98.1.sh" build --release --features kv-all --bin kvbench

for ((trial=1; trial<=TRIALS; trial++)); do
  JOBS=()
  for durability in "${DURABILITIES[@]}"; do
    for workload in "${WORKLOADS[@]}"; do
      for engine in "${ENGINES[@]}"; do
        [[ "$engine" == lkv && "$durability" == relaxed ]] && continue
        [[ "$engine" == manifold && "$durability" == relaxed ]] && continue
        [[ "$engine" == jammdb && "$durability" == relaxed ]] && continue
        [[ "$engine" == lsmdb && "$durability" == relaxed ]] && continue
        [[ "$durability" == sync && ( "$engine" == paritydb-hash || "$engine" == paritydb-btree ) ]] && continue
        [[ "$engine" == lkv && "$workload" == range-scan ]] && continue
        [[ "$engine" == paritydb-hash && "$workload" == range-scan ]] && continue
        JOBS+=("$engine|$durability|$workload")
      done
    done
  done

  while IFS= read -r job; do
    IFS='|' read -r engine durability workload <<< "$job"
    echo "trial=$trial engine=$engine durability=$durability workload=$workload" >&2
    "$BIN"       --engine "$engine"       --durability "$durability"       --workload "$workload"       --records "$RECORDS"       --ops "$OPS"       --value-bytes 256       --txn-size 100       --scan-len 100       --trial "$trial"       --seed 1592606758       --scenario baseline-core       --root "$DATA_DIR"       --output "$OUT"
  done < <(printf '%s
' "${JOBS[@]}" | shuf)
done

echo "$OUT"
