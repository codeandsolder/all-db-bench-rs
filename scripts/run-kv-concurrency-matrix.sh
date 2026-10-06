#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke)
    TRIALS=1; RECORDS=5000; OPS=3000
    CLIENTS=(1 4 8)
    CORE_WORKLOADS=(point-read read-heavy tiny-txn write-burst)
    RANGE_CLIENTS=(1 8)
    DELETE_CLIENTS=(1 8)
    RELAXED_CLIENTS=(1 8)
    ;;
  quick)
    TRIALS=3; RECORDS=100000; OPS=50000
    CLIENTS=(1 2 4 8)
    CORE_WORKLOADS=(point-read read-heavy balanced tiny-txn write-burst churn)
    RANGE_CLIENTS=(1 4 8)
    DELETE_CLIENTS=(1 4 8)
    RELAXED_CLIENTS=(1 4 8)
    ;;
  full)
    TRIALS=7; RECORDS=1000000; OPS=250000
    CLIENTS=(1 2 4 8 16)
    CORE_WORKLOADS=(point-read read-heavy balanced tiny-txn write-burst churn)
    RANGE_CLIENTS=(1 2 4 8 16)
    DELETE_CLIENTS=(1 2 4 8 16)
    RELAXED_CLIENTS=(1 2 4 8 16)
    ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
# shellcheck source=kv-matrix-policy.sh
source "$ROOT/scripts/kv-matrix-policy.sh"
CPUS=$(getconf _NPROCESSORS_ONLN)
LOAD1=$(awk '{print $1}' /proc/loadavg)
if [[ "${ALLOW_BUSY:-0}" != 1 ]] && ! awk -v l="$LOAD1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 1.5) }'; then
  echo "refusing concurrency benchmark on busy host: load1=$LOAD1 visible_cpus=$CPUS" >&2
  exit 75
fi

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-kv-concurrency-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr} "$DATA_DIR"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
cat > "$RUN_DIR/support.json" <<'JSON'
{
  "lane": "kv-concurrency",
  "shared_database": true,
  "total_ops_fixed_across_client_counts": true,
  "lsmdb_range_policy": "cap native lsm-db 1.0.0 bounded scans at about 50M expected pre-range entries using kv-matrix-policy.sh; never increase an existing lane budget",
  "unsupported": [
    {
      "engine": "lkv",
      "scope": "all concurrency workloads",
      "reason": "lkv 0.2.1 write transactions require mutable Database access and expose no cloneable/shared writer handle; an external mutex would benchmark harness serialization rather than engine concurrency"
    },
    {
      "engine": "paritydb-hash",
      "scope": "range-scan",
      "reason": "hash-column mode is intentionally unordered; paritydb-btree is the ordered ParityDB configuration"
    }
  ]
}
JSON

TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
BIN="$TARGET_DIR/release/kvconcurrency"
CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" \
  build --release --features kv-all --bin kvconcurrency || exit $?

# lkv is deliberately absent: 0.2.1 has no native clone/shared writer handle.
ENGINES=(redb fjall surrealkv heed sled manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
ORDERED_ENGINES=(redb fjall surrealkv heed sled manifold turbokv paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
JOBS=()

primary_durability() {
  case "$1" in
    paritydb-hash|paritydb-btree) echo relaxed ;;
    *) echo sync ;;
  esac
}

relaxed_supported() {
  case "$1" in
    manifold|jammdb|lsmdb) return 1 ;;
    *) return 0 ;;
  esac
}

add_job() {
  local scenario=$1 engine=$2 durability=$3 workload=$4 clients=$5 trial=$6
  JOBS+=("$scenario|$engine|$durability|$workload|$clients|$trial")
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

for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    dur=$(primary_durability "$engine")
    for workload in "${CORE_WORKLOADS[@]}"; do
      for clients in "${CLIENTS[@]}"; do
        add_job primary "$engine" "$dur" "$workload" "$clients" "$trial"
      done
    done
  done

  for engine in "${ORDERED_ENGINES[@]}"; do
    dur=$(primary_durability "$engine")
    for clients in "${RANGE_CLIENTS[@]}"; do
      add_job range "$engine" "$dur" range-scan "$clients" "$trial"
    done
  done

  for engine in "${ENGINES[@]}"; do
    dur=$(primary_durability "$engine")
    for clients in "${DELETE_CLIENTS[@]}"; do
      add_job delete "$engine" "$dur" delete-burst "$clients" "$trial"
    done
  done

  for engine in "${ENGINES[@]}"; do
    [[ "$(primary_durability "$engine")" == relaxed ]] && continue
    relaxed_supported "$engine" || continue
    for workload in read-heavy write-burst; do
      for clients in "${RELAXED_CLIENTS[@]}"; do
        add_job relaxed "$engine" relaxed "$workload" "$clients" "$trial"
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
  IFS='|' read -r scenario engine durability workload clients trial <<< "$job"

  ops=$OPS
  scan=100
  if [[ "$workload" == range-scan ]]; then
    ops=$(( OPS / scan ))
    (( ops < clients )) && ops=$clients
    ops=$(kv_effective_ops "$engine" "$workload" "$ops" "$RECORDS") || exit 2
  elif [[ "$workload" == tiny-txn ]]; then
    # Shared policy applies the one-key-transaction sizing once; do not
    # pre-divide here or the helper would scale this lane twice.
    ops=$(kv_effective_ops "$engine" "$workload" "$OPS" "$RECORDS") || exit 2
    (( ops < clients )) && ops=$clients
  elif [[ "$workload" == delete-burst && "$ops" -gt "$RECORDS" ]]; then
    ops=$RECORDS
  fi

  case_id="t${trial}-${scenario}-${engine}-${durability}-${workload}-c${clients}-n${RECORDS}-o${ops}"
  out="$RUN_DIR/cases/$case_id.json"
  [[ -s "$out" ]] && continue
  err="$RUN_DIR/stderr/$case_id.log"
  echo "[$INDEX/$TOTAL] $case_id" >&2

  "$BIN" \
    --engine "$engine" --durability "$durability" --workload "$workload" \
    --records "$RECORDS" --ops "$ops" --clients "$clients" \
    --value-bytes 256 --value-pattern pseudo-random \
    --key-bytes 8 --key-shape sequential --access-pattern auto \
    --txn-size 100 --scan-len "$scan" --warmup-reads 5000 \
    --trial "$trial" --seed 1592606758 \
    --scenario "concurrency-$scenario" \
    --root "$DATA_DIR" --output "$out" 2>"$err"
  rc=$?

  if (( rc != 0 )) || [[ ! -s "$out" ]]; then
    rm -f "$out"
    clear_failure "$case_id"
    jq -cn \
      --arg case_id "$case_id" --arg stderr "$err" --argjson rc "$rc" \
      '{case_id:$case_id, returncode:$rc, stderr:$stderr}' \
      >> "$RUN_DIR/failures.ndjson"
  else
    clear_failure "$case_id"
    if [[ ! -s "$err" ]]; then
      rm -f "$err"
    fi
  fi
done

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
printf 'run=%s cpus=%s total=%s failures=%s results=%s
' \
  "$RUN_ID" "$CPUS" "$TOTAL" "$FAILURES" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 ? 1 : 0 ))
