#!/usr/bin/env bash
set -u -o pipefail
PROFILE=${1:-quick}
case "$PROFILE" in
  smoke) TRIALS=1; RECORDS=1000; DELAYS=(0.002 0.010); TXNS=(1 16) ;;
  quick) TRIALS=3; RECORDS=10000; DELAYS=(0.001 0.005 0.020 0.100); TXNS=(1 16 100) ;;
  full) TRIALS=7; RECORDS=100000; DELAYS=(0.001 0.005 0.020 0.100 0.500); TXNS=(1 16 100 1024) ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac
ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-crash-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"; DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,progress,child} "$DATA_DIR"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}"
BIN="$TARGET_DIR/release/kvbench"
"$ROOT/scripts/cargo-local-1.98.1.sh" build --release --features kv-all --bin kvbench || exit $?
ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv rocksdb mdbx persy roughdb jammdb lsmdb)
DURS=(relaxed sync)
FAILURES=0
TOTAL=0

for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    for dur in "${DURS[@]}"; do
      [[ "$engine" == lkv && "$dur" == relaxed ]] && continue
      [[ "$engine" == manifold && "$dur" == relaxed ]] && continue
      [[ "$engine" == jammdb && "$dur" == relaxed ]] && continue
      [[ "$engine" == lsmdb && "$dur" == relaxed ]] && continue
      for txn in "${TXNS[@]}"; do
        for delay in "${DELAYS[@]}"; do
          TOTAL=$((TOTAL+1))
          tag=$(printf '%s' "$delay" | tr '.' 'p')
          case_id="t${trial}-${engine}-${dur}-tx${txn}-d${tag}"
          out="$RUN_DIR/cases/$case_id.json"; [[ -s "$out" ]] && continue
          db_name="crash-$case_id"
          progress="$RUN_DIR/progress/$case_id.txt"
          child_out="$RUN_DIR/child/$case_id.out"
          child_err="$RUN_DIR/stderr/$case_id.child.log"
          rm -f "$progress" "$child_out"

          # Stable base image. We keep it, then mutate it in the process that will be SIGKILLed.
          "$BIN" --engine "$engine" --durability "$dur" --workload point-read \
            --records "$RECORDS" --ops 1 --value-bytes 256 --txn-size "$txn" --scan-len 100 \
            --trial "$trial" --seed 1592606758 --scenario crash-prepare --db-name "$db_name" \
            --root "$DATA_DIR" --output /dev/null --keep-db --warmup-reads 0 \
            > /dev/null 2>"$RUN_DIR/stderr/$case_id.prepare.log"
          if (( $? != 0 )); then FAILURES=$((FAILURES+1)); continue; fi

          "$BIN" --engine "$engine" --durability "$dur" --workload write-burst \
            --records "$RECORDS" --ops 1000000000 --value-bytes 256 --txn-size "$txn" --scan-len 100 \
            --trial "$trial" --seed 1592606758 --scenario crash-child --db-name "$db_name" \
            --root "$DATA_DIR" --output "$child_out" --keep-db --reuse-db --skip-prefill \
            --warmup-reads 0 --progress-file "$progress" \
            > /dev/null 2>"$child_err" &
          pid=$!

          # Start the kill delay only after at least one transaction has definitely returned.
          ready=0
          for _ in $(seq 1 5000); do
            if [[ -s "$progress" ]]; then ready=1; break; fi
            if ! kill -0 "$pid" 2>/dev/null; then break; fi
            sleep 0.001
          done
          if (( ready == 0 )); then
            kill -KILL "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            FAILURES=$((FAILURES+1))
            echo "$case_id child never reached first acknowledged transaction" >&2
            continue
          fi

          sleep "$delay"
          kill -KILL "$pid" 2>/dev/null || true
          wait "$pid" 2>/dev/null || true

          acked=$(tr -dc '0-9' < "$progress")
          [[ -n "$acked" ]] || acked=0
          expected=$((RECORDS + acked))
          tail=$((txn * 2))
          (( tail < 16 )) && tail=16

          "$BIN" --engine "$engine" --durability "$dur" --workload point-read \
            --records "$RECORDS" --ops 1 --value-bytes 256 --txn-size "$txn" --scan-len 100 \
            --trial "$trial" --seed 1592606758 --scenario "crash-sigkill-d$tag" --db-name "$db_name" \
            --root "$DATA_DIR" --output "$out" --reuse-db --skip-prefill --warmup-reads 0 \
            --verify-prefix-records "$expected" --verify-tail-records "$tail" \
            2>"$RUN_DIR/stderr/$case_id.verify.log"
          rc=$?
          if (( rc != 0 )); then
            FAILURES=$((FAILURES+1)); rm -f "$out"
          elif ! jq -e '.verification.verification_ok == true' "$out" >/dev/null; then
            FAILURES=$((FAILURES+1))
            echo "$case_id recovery verification FAILED" >&2
          fi
        done
      done
    done
  done
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
echo "run=$RUN_ID cases=$TOTAL failures=$FAILURES results=$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 ? 1 : 0 ))
