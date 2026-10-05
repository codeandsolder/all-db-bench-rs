#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke)
    TRIALS=1; RECORDS=200; DELAYS=(0.002 0.010); TXNS=(1 16)
    ;;
  quick)
    TRIALS=3; RECORDS=5000; DELAYS=(0.001 0.005 0.020 0.100); TXNS=(1 16 100)
    ;;
  full)
    TRIALS=7; RECORDS=50000; DELAYS=(0.001 0.005 0.020 0.100 0.500); TXNS=(1 16 100 1024)
    ;;
  *)
    echo "usage: $0 [smoke|quick|full]" >&2
    exit 2
    ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-record-crash-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,progress,child} "$DATA_DIR"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"

TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
else
  BIN="$TARGET_DIR/release/recordbench"
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" \
    build --release --features record --bin recordbench || exit $?
fi

ENGINES=(surrealdb turso sqlite)
DURS=(relaxed sync)
TOTAL=0
RELAXED_ACK_LOSS_CASES=0

clear_failure() {
  local case_id=$1
  local failures="$RUN_DIR/failures.ndjson"
  [[ -f "$failures" ]] || return 0
  local tmp="${failures}.tmp"
  jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' \
    "$failures" > "$tmp"
  mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}

record_failure() {
  local case_id=$1 stage=$2 rc=$3 stderr=$4
  clear_failure "$case_id"
  jq -cn \
    --arg case_id "$case_id" \
    --arg stage "$stage" \
    --arg stderr "$stderr" \
    --argjson rc "$rc" \
    '{case_id:$case_id, stage:$stage, returncode:$rc, stderr:$stderr}' \
    >> "$RUN_DIR/failures.ndjson"
}

clear_relaxed_loss() {
  local case_id=$1
  local losses="$RUN_DIR/relaxed-ack-losses.ndjson"
  [[ -f "$losses" ]] || return 0
  local tmp="${losses}.tmp"
  jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)'     "$losses" > "$tmp"
  mv "$tmp" "$losses"
  [[ -s "$losses" ]] || rm -f "$losses"
}

for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    for dur in "${DURS[@]}"; do
      for txn in "${TXNS[@]}"; do
        for delay in "${DELAYS[@]}"; do
          TOTAL=$((TOTAL + 1))
          tag=$(printf '%s' "$delay" | tr '.' 'p')
          case_id="t${trial}-${engine}-${dur}-tx${txn}-d${tag}"
          out="$RUN_DIR/cases/$case_id.json"
          if [[ -s "$out" ]]; then
            clear_failure "$case_id"
            if jq -e '.verification.verification_ok == true' "$out" >/dev/null; then
              clear_relaxed_loss "$case_id"
            fi
            continue
          fi

          db_name="record-crash-$case_id"
          progress="$RUN_DIR/progress/$case_id.txt"
          child_out="$RUN_DIR/child/$case_id.out"
          prepare_err="$RUN_DIR/stderr/$case_id.prepare.log"
          child_err="$RUN_DIR/stderr/$case_id.child.log"
          verify_err="$RUN_DIR/stderr/$case_id.verify.log"
          rm -f "$progress" "$child_out"

          echo "[$case_id] prepare" >&2
          "$BIN" --engine "$engine" --durability "$dur" --workload point-read \
            --records "$RECORDS" --ops 1 --payload-bytes 256 --txn-size "$txn" \
            --trial "$trial" --seed 1592606758 --scenario record-crash-prepare \
            --db-name "$db_name" --root "$DATA_DIR" --output /dev/null \
            --keep-db --warmup-reads 0 > /dev/null 2>"$prepare_err"
          rc=$?
          if (( rc != 0 )); then
            record_failure "$case_id" prepare "$rc" "$prepare_err"
            continue
          fi

          echo "[$case_id] writer" >&2
          "$BIN" --engine "$engine" --durability "$dur" --workload write-burst \
            --records "$RECORDS" --ops 1000000000 --payload-bytes 256 --txn-size "$txn" \
            --trial "$trial" --seed 1592606758 --scenario record-crash-child \
            --db-name "$db_name" --root "$DATA_DIR" --output "$child_out" \
            --keep-db --reuse-db --skip-prefill --warmup-reads 0 \
            --progress-file "$progress" > /dev/null 2>"$child_err" &
          pid=$!

          ready=0
          for _ in $(seq 1 5000); do
            if [[ -s "$progress" ]]; then
              ready=1
              break
            fi
            if ! kill -0 "$pid" 2>/dev/null; then
              break
            fi
            sleep 0.001
          done
          if (( ready == 0 )); then
            kill -KILL "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            record_failure "$case_id" writer-ready 1 "$child_err"
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

          echo "[$case_id] verify acked=$acked" >&2
          "$BIN" --engine "$engine" --durability "$dur" --workload point-read \
            --records "$RECORDS" --ops 1 --payload-bytes 256 --txn-size "$txn" \
            --trial "$trial" --seed 1592606758 --scenario "record-crash-sigkill-d$tag" \
            --db-name "$db_name" --root "$DATA_DIR" --output "$out" \
            --reuse-db --skip-prefill --warmup-reads 0 \
            --verify-prefix-records "$expected" --verify-tail-records "$tail" \
            2>"$verify_err"
          rc=$?
          if (( rc != 0 )) || [[ ! -s "$out" ]]; then
            rm -f "$out"
            record_failure "$case_id" verify "$rc" "$verify_err"
            continue
          fi

          if ! jq -e '
            .verification.prefix_present_after_gap == 0
            and .verification.tail_present_after_gap == 0
            and .verification.transaction_atomic_tail == true
          ' "$out" >/dev/null; then
            record_failure "$case_id" structural-verification 1 "$verify_err"
            echo "$case_id recovery STRUCTURAL verification FAILED" >&2
            continue
          fi

          if ! jq -e '.verification.verification_ok == true' "$out" >/dev/null; then
            if [[ "$dur" == sync ]]; then
              record_failure "$case_id" durable-verification 1 "$verify_err"
              echo "$case_id durable recovery verification FAILED" >&2
              continue
            fi
            clear_relaxed_loss "$case_id"
            jq -c --arg case_id "$case_id" \
              '{case_id:$case_id, engine:.engine, durability:.durability, verification:.verification}' \
              "$out" >> "$RUN_DIR/relaxed-ack-losses.ndjson"
            echo "$case_id relaxed lane lost a contiguous acknowledged suffix; recorded, not a harness failure" >&2
          fi
          clear_failure "$case_id"
          if jq -e '.verification.verification_ok == true' "$out" >/dev/null; then
            clear_relaxed_loss "$case_id"
          fi
        done
      done
    done
  done
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 |
  sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"

if [[ -s "$RUN_DIR/failures.ndjson" ]]; then
  FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson")
else
  FAILURES=0
fi
if [[ -s "$RUN_DIR/relaxed-ack-losses.ndjson" ]]; then
  RELAXED_ACK_LOSS_CASES=$(wc -l < "$RUN_DIR/relaxed-ack-losses.ndjson")
else
  RELAXED_ACK_LOSS_CASES=0
fi
echo "run=$RUN_ID cases=$TOTAL failures=$FAILURES relaxed_ack_loss_cases=$RELAXED_ACK_LOSS_CASES results=$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 ? 1 : 0 ))
