#!/usr/bin/env bash
set -u -o pipefail
PROFILE=${1:-quick}
CACHE=${2:-warm}
case "$PROFILE" in
  smoke) TRIALS=1; RECORDS=1000; OPS=200 ;;
  quick) TRIALS=3; RECORDS=100000; OPS=5000 ;;
  full) TRIALS=7; RECORDS=1000000; OPS=25000 ;;
  *) echo "usage: $0 [smoke|quick|full] [warm|cold]" >&2; exit 2 ;;
esac
case "$CACHE" in
  warm) ;;
  cold)
    if (( EUID != 0 )); then
      echo "cold-cache lane requires root for: sync; echo 3 > /proc/sys/vm/drop_caches" >&2
      echo "Run this script as root on an otherwise quiet host; it deliberately does not fake cache eviction." >&2
      exit 77
    fi
    ;;
  *) echo "cache mode must be warm or cold" >&2; exit 2 ;;
esac
ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-reopen-$CACHE-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"; DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr} "$DATA_DIR"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}"
BIN="$TARGET_DIR/release/kvbench"
"$ROOT/scripts/cargo-local-1.98.1.sh" build --release --features kv-all --bin kvbench || exit $?

ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
DURS=(relaxed sync)
WORKLOADS=(point-read range-scan)
FAILURES=0
for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    for dur in "${DURS[@]}"; do
      [[ "$engine" == lkv && "$dur" == relaxed ]] && continue
      [[ "$engine" == manifold && "$dur" == relaxed ]] && continue
      [[ "$engine" == jammdb && "$dur" == relaxed ]] && continue
      [[ "$engine" == lsmdb && "$dur" == relaxed ]] && continue
      [[ "$dur" == sync && ( "$engine" == paritydb-hash || "$engine" == paritydb-btree ) ]] && continue
      for workload in "${WORKLOADS[@]}"; do
        [[ "$engine" == lkv && "$workload" == range-scan ]] && continue
        [[ "$engine" == paritydb-hash && "$workload" == range-scan ]] && continue
        db_name="reopen-${trial}-${engine}-${dur}-${workload}"
        case_id="t${trial}-reopen-${CACHE}-${engine}-${dur}-${workload}-n${RECORDS}"
        out="$RUN_DIR/cases/$case_id.json"; [[ -s "$out" ]] && continue

        # Prepare exactly the DB shape the reopen process will consume. The one read is discarded.
        "$BIN" --engine "$engine" --durability "$dur" --workload "$workload" \
          --records "$RECORDS" --ops 1 --value-bytes 256 --txn-size 100 --scan-len 100 \
          --trial "$trial" --seed 1592606758 --scenario reopen-prepare --db-name "$db_name" \
          --root "$DATA_DIR" --output /dev/null --keep-db --warmup-reads 0 \
          > /dev/null 2>"$RUN_DIR/stderr/$case_id.prepare.log"
        rc=$?
        if (( rc != 0 )); then
          FAILURES=$((FAILURES+1))
          continue
        fi

        if [[ "$CACHE" == cold ]]; then
          sync
          echo 3 > /proc/sys/vm/drop_caches
        fi

        "$BIN" --engine "$engine" --durability "$dur" --workload "$workload" \
          --records "$RECORDS" --ops "$OPS" --value-bytes 256 --txn-size 100 --scan-len 100 \
          --trial "$trial" --seed 1592606758 --scenario "reopen-$CACHE" --db-name "$db_name" \
          --root "$DATA_DIR" --output "$out" --reuse-db --skip-prefill --warmup-reads 0 \
          2>"$RUN_DIR/stderr/$case_id.log"
        rc=$?
        if (( rc != 0 )); then
          FAILURES=$((FAILURES+1)); rm -f "$out"
        fi
      done
    done
  done
done
find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" \
  --expect-trials "$TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md" || true
echo "$RUN_DIR"
exit $(( FAILURES > 0 ? 1 : 0 ))
