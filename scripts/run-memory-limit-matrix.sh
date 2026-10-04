#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
case "$PROFILE" in
  smoke) TRIALS=1; LIMIT_PCTS=(25 50); RECORDS=100000; OPS=5000 ;;
  quick) TRIALS=3; LIMIT_PCTS=(25 50 75); RECORDS=500000; OPS=50000 ;;
  full)  TRIALS=5; LIMIT_PCTS=(15 25 50 75); RECORDS=1500000; OPS=250000 ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
BIN="$TARGET_DIR/release/kvbench"
if [[ ! -x "$BIN" ]]; then
  echo "build $BIN as the normal benchmark user before invoking this root-only runner" >&2
  echo "CARGO_TARGET_DIR=$TARGET_DIR $ROOT/scripts/cargo-local-1.98.1.sh build --release --features kv-all --bin kvbench" >&2
  exit 66
fi
if (( EUID != 0 )); then
  echo "memory-limit lane requires root for transient cgroup MemoryMax/MemorySwapMax controls; no userspace pressure-process workaround is used" >&2
  exit 77
fi
command -v systemd-run >/dev/null || { echo "systemd-run required" >&2; exit 69; }

CPUS=$(getconf _NPROCESSORS_ONLN)
LOAD1=$(awk '{print $1}' /proc/loadavg)
if [[ "${ALLOW_BUSY:-0}" != 1 ]] && ! awk -v l="$LOAD1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 1.5) }'; then
  echo "refusing memory-limit benchmark on busy host: load1=$LOAD1 visible_cpus=$CPUS" >&2
  exit 75
fi

MEM_KIB=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-memory-limit-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,cgroup} "$DATA_DIR"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"

ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
WORKLOADS=(point-read read-heavy churn)
primary_durability() {
  case "$1" in
    paritydb-hash|paritydb-btree) echo relaxed ;;
    *) echo sync ;;
  esac
}

JOBS=()
for trial in $(seq 1 "$TRIALS"); do
  for pct in "${LIMIT_PCTS[@]}"; do
    for workload in "${WORKLOADS[@]}"; do
      for engine in "${ENGINES[@]}"; do
        dur=$(primary_durability "$engine")
        JOBS+=("$trial|$pct|$workload|$engine|$dur")
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
  IFS='|' read -r trial pct workload engine dur <<< "$job"
  limit_bytes=$(( MEM_KIB * 1024 * pct / 100 ))
  case_id="t${trial}-mem${pct}pct-noswap-${engine}-${dur}-${workload}"
  out="$RUN_DIR/cases/$case_id.json"
  [[ -s "$out" ]] && continue
  echo "[$INDEX/$TOTAL] $case_id MemoryMax=$limit_bytes" >&2

  unit="dbbench-${RUN_ID//[^A-Za-z0-9]/-}-${INDEX}"
  err="$RUN_DIR/stderr/$case_id.log"
  systemd-run --quiet --wait --unit="$unit" \
    -p "MemoryMax=$limit_bytes" -p MemorySwapMax=0 -p OOMPolicy=stop \
    "$BIN" --engine "$engine" --durability "$dur" --workload "$workload" \
      --records "$RECORDS" --ops "$OPS" --value-bytes 1024 --value-pattern pseudo-random \
      --key-bytes 16 --key-shape sequential --access-pattern hot80 --txn-size 100 \
      --trial "$trial" --seed 1592606758 --scenario "memory-${pct}pct-noswap" \
      --root "$DATA_DIR" --output "$out" 2>"$err"
  rc=$?

  systemctl show "$unit" \
    -p Result -p ExecMainCode -p ExecMainStatus -p MemoryPeak -p MemoryCurrent \
    > "$RUN_DIR/cgroup/$case_id.txt" 2>/dev/null || true
  systemctl reset-failed "$unit" 2>/dev/null || true

  if (( rc != 0 )) || [[ ! -s "$out" ]]; then
    FAILURES=$((FAILURES + 1))
    rm -f "$out"
    jq -cn --arg case_id "$case_id" --arg stderr "$err" --arg cgroup "$RUN_DIR/cgroup/$case_id.txt" --argjson rc "$rc" \
      '{case_id:$case_id, returncode:$rc, stderr:$stderr, cgroup:$cgroup}' >> "$RUN_DIR/failures.ndjson"
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
