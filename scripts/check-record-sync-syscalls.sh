#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

MAIN_BIN=${RECORD_BENCH_BIN:-}
ROCKS_BIN=${ROCKS_RECORD_BENCH_BIN:-}

if [[ -z "$MAIN_BIN" ]]; then
  TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
  MAIN_BIN="$TARGET_DIR/release/recordbench"
  "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features record --bin recordbench
fi
if [[ -z "$ROCKS_BIN" ]]; then
  ROCKS_TARGET_DIR=${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}
  ROCKS_BIN="$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordbench"
  "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked \
    --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" \
    --target-dir "$ROCKS_TARGET_DIR" --bin surrealdb-rocksdb-recordbench
fi

[[ -x "$MAIN_BIN" ]] || { echo "record benchmark binary is not executable: $MAIN_BIN" >&2; exit 2; }
[[ -x "$ROCKS_BIN" ]] || { echo "RocksDB record benchmark binary is not executable: $ROCKS_BIN" >&2; exit 2; }
command -v strace >/dev/null || { echo "strace is required" >&2; exit 2; }

PROBE_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/dbbench-record-sync-probe.XXXXXX")
TRACE_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/dbbench-record-sync-trace.XXXXXX")
trap 'rm -rf "$PROBE_ROOT" "$TRACE_ROOT"' EXIT

count_syncs() {
  local prefix=$1 total=0 f n
  shopt -s nullglob
  for f in "$prefix"*; do
    n=$(rg -c '(^|[^[:alpha:]_])(fsync|fdatasync|msync|sync_file_range|syncfs)\(' "$f" 2>/dev/null || true)
    total=$((total + ${n:-0}))
  done
  printf '%d\n' "$total"
}

trace_one() {
  local engine=$1 durability=$2 bin
  local prefix="$TRACE_ROOT/$engine-$durability"
  if [[ "$engine" == surrealdb-rocksdb ]]; then bin=$ROCKS_BIN; else bin=$MAIN_BIN; fi
  rm -rf "$PROBE_ROOT/$engine-$durability"
  strace -qq -ff -e trace=fsync,fdatasync,msync,sync_file_range,syncfs -o "$prefix" \
    "$bin" --engine "$engine" --durability "$durability" --workload tiny-txn \
      --records 8 --ops 1 --payload-bytes 64 --txn-size 1 --warmup-reads 0 \
      --trial 1 --seed 1592606758 --scenario durability-syscall-probe \
      --root "$PROBE_ROOT/$engine-$durability" --output /dev/null --keep-db >/dev/null
  count_syncs "$prefix"
}

printf '%-22s %10s %10s %s\n' engine relaxed sync verdict
fail=0
for engine in surrealdb turso sqlite surrealdb-rocksdb; do
  relaxed=$(trace_one "$engine" relaxed)
  sync=$(trace_one "$engine" sync)
  verdict=ok
  if (( sync <= relaxed )); then
    verdict=CHECK
    fail=1
  fi
  printf '%-22s %10d %10d %s\n' "$engine" "$relaxed" "$sync" "$verdict"
done

if (( fail )); then
  echo "record durability syscall probe found a sync lane without a larger explicit barrier count" >&2
  exit 1
fi
