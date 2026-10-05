#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke)
    TRIALS=1; RECORDS=5000; OPS=3000
    LOAD_PCTS=(0 50 100)
    WORKLOADS=(point-read read-heavy write-burst)
    ;;
  quick)
    TRIALS=3; RECORDS=100000; OPS=50000
    LOAD_PCTS=(0 25 50 75 100)
    WORKLOADS=(point-read read-heavy write-burst)
    ;;
  full)
    TRIALS=7; RECORDS=1000000; OPS=250000
    LOAD_PCTS=(0 12 25 50 75 100)
    WORKLOADS=(point-read read-heavy balanced write-burst churn)
    ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}

expand_cpu_list() {
  local spec part start end cpu
  spec=$(awk '/^Cpus_allowed_list:/ {print $2}' /proc/self/status)
  IFS=',' read -ra parts <<< "$spec"
  for part in "${parts[@]}"; do
    if [[ "$part" == *-* ]]; then
      start=${part%-*}
      end=${part#*-}
      for ((cpu=start; cpu<=end; cpu++)); do
        printf '%s
' "$cpu"
      done
    else
      printf '%s
' "$part"
    fi
  done
}
mapfile -t CPU_IDS < <(expand_cpu_list)
CPUS=${#CPU_IDS[@]}
(( CPUS > 0 )) || { echo "no CPUs in current affinity mask" >&2; exit 2; }
LOAD1=$(awk '{print $1}' /proc/loadavg)
if [[ "${ALLOW_BUSY:-0}" != 1 ]] && ! awk -v l="$LOAD1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 0.5) }'; then
  echo "refusing CPU-contention benchmark on already-busy host: load1=$LOAD1 visible_cpus=$CPUS" >&2
  exit 75
fi

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-cpu-contention-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,pressure} "$DATA_DIR"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
cat > "$RUN_DIR/support.json" <<JSON
{
  "lane": "cpu-contention",
  "allowed_cpu_ids": "$(IFS=,; echo "${CPU_IDS[*]}")",
  "pressure_method": "one taskset-pinned worker per allowed CPU; each worker uses a 20 ms period at the requested busy duty cycle",
  "pressure_scope": "pressure is sustained for the full benchmark process invocation, including open/prefill/warmup and measured phase",
  "load_percent_interpretation": "requested background duty cycle on every allowed logical CPU, not measured total host utilization"
}
JSON

TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
else
  BIN="$TARGET_DIR/release/kvbench"
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" \
    build --release --features kv-all --bin kvbench || exit $?
fi

ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
JOBS=()
BURN_PIDS=()

primary_durability() {
  case "$1" in
    paritydb-hash|paritydb-btree) echo relaxed ;;
    *) echo sync ;;
  esac
}

clear_failure() {
  local case_id=$1
  local failures="$RUN_DIR/failures.ndjson"
  [[ -f "$failures" ]] || return 0
  local tmp="${failures}.tmp"
  jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)'     "$failures" > "$tmp"
  mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}

cleanup_burners() {
  if (( ${#BURN_PIDS[@]} )); then
    kill "${BURN_PIDS[@]}" 2>/dev/null || true
    wait "${BURN_PIDS[@]}" 2>/dev/null || true
  fi
  BURN_PIDS=()
}
trap cleanup_burners EXIT INT TERM

BURN_WORKERS=0
start_burners() {
  local pct=$1
  cleanup_burners
  BURN_WORKERS=0
  if (( pct == 0 )); then
    # Give scheduler queues a brief quiet interval after the prior randomized
    # pressure case before launching the zero-pressure baseline.
    sleep 0.25
    return 0
  fi

  # Use the same duty cycle on every CPU. Fully occupying only a subset lets
  # the scheduler move the benchmark to idle CPUs and defeats intermediate
  # contention points.
  local cpu
  for cpu in "${CPU_IDS[@]}"; do
    taskset -c "$cpu" python3 "$ROOT/scripts/cpu-pressure-worker.py" "$pct"       >/dev/null 2>&1 &
    BURN_PIDS+=("$!")
    BURN_WORKERS=$((BURN_WORKERS + 1))
  done
  sleep 0.25
}

for trial in $(seq 1 "$TRIALS"); do
  for pct in "${LOAD_PCTS[@]}"; do
    for workload in "${WORKLOADS[@]}"; do
      for engine in "${ENGINES[@]}"; do
        JOBS+=("$trial|$pct|$workload|$engine")
      done
    done
  done
done

mapfile -t ORDERED < <(printf '%s\n' "${JOBS[@]}" | shuf)
TOTAL=${#ORDERED[@]}
FAILURES=0
INDEX=0
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1))
  IFS='|' read -r trial pct workload engine <<< "$job"
  dur=$(primary_durability "$engine")
  case_id="t${trial}-cpu${pct}pct-${engine}-${dur}-${workload}"
  out="$RUN_DIR/cases/$case_id.json"
  if [[ -s "$out" ]]; then
    clear_failure "$case_id"
    continue
  fi
  err="$RUN_DIR/stderr/$case_id.log"

  start_burners "$pct"
  workers=$BURN_WORKERS
  cat /proc/pressure/cpu > "$RUN_DIR/pressure/$case_id.before.txt"
  echo "[$INDEX/$TOTAL] $case_id burners=$workers/$CPUS" >&2

  "$BIN" \
    --engine "$engine" --durability "$dur" --workload "$workload" \
    --records "$RECORDS" --ops "$OPS" --value-bytes 256 \
    --txn-size 100 --scan-len 100 --warmup-reads 5000 \
    --trial "$trial" --seed 1592606758 \
    --scenario "cpu-contention-${pct}pct" \
    --root "$DATA_DIR" --output "$out" 2>"$err"
  rc=$?
  cat /proc/pressure/cpu > "$RUN_DIR/pressure/$case_id.after.txt"
  cleanup_burners

  if (( rc != 0 )) || [[ ! -s "$out" ]]; then
    rm -f "$out"
    clear_failure "$case_id"
    jq -cn \
      --arg case_id "$case_id" --arg stderr "$err" --argjson rc "$rc" \
      --argjson pct "$pct" --argjson burners "$workers" \
      '{case_id:$case_id, returncode:$rc, cpu_load_percent:$pct, burners:$burners, stderr:$stderr}' \
      >> "$RUN_DIR/failures.ndjson"
  else
    clear_failure "$case_id"
    if [[ ! -s "$err" ]]; then
      rm -f "$err"
    fi
  fi
done
cleanup_burners

find "$RUN_DIR/cases" -type f -name '*.json' -print0 |
  sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" \
  --expect-trials "$TRIALS" \
  --json-out "$RUN_DIR/summary.json" \
  --markdown-out "$RUN_DIR/summary.md" || true

if [[ -s "$RUN_DIR/failures.ndjson" ]]; then
  FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson")
else
  FAILURES=0
fi
echo "run=$RUN_ID cpus=$CPUS total=$TOTAL failures=$FAILURES results=$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 ? 1 : 0 ))
