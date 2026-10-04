#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}"
BIN="$TARGET_DIR/release/kvbench"
"$ROOT/scripts/cargo-local-1.98.1.sh" build --release --features kv-all --bin kvbench
PROBE_ROOT="$ROOT/data/sync-syscall-probe"
rm -rf "$PROBE_ROOT"
mkdir -p "$PROBE_ROOT"
TRACE_ROOT=$(mktemp -d /tmp/dbbench-sync-trace.XXXXXX)
trap 'rm -rf "$TRACE_ROOT" "$PROBE_ROOT"' EXIT

count_syncs() {
  local prefix=$1
  local total=0
  local f n
  shopt -s nullglob
  for f in "$prefix"*; do
    n=$(rg -c '(^|[^[:alpha:]_])(fsync|fdatasync|msync|sync_file_range)\(' "$f" 2>/dev/null || true)
    total=$((total + ${n:-0}))
  done
  echo "$total"
}

trace_one() {
  local engine=$1 durability=$2 prefix="$TRACE_ROOT/$engine-$durability"
  strace -qq -ff -e trace=fsync,fdatasync,msync,sync_file_range -o "$prefix" \
    "$BIN" --engine "$engine" --durability "$durability" --workload tiny-txn \
      --records 10 --ops 1 --value-bytes 64 --txn-size 1 --trial 1 \
      --root "$PROBE_ROOT" >/dev/null
  count_syncs "$prefix"
}

printf '%-12s %10s %10s %s\n' engine relaxed sync verdict
# Hard assertion where the durable mode should add explicit barriers.
for engine in redb fjall surrealkv heed sled turbokv rocksdb mdbx roughdb; do
  relaxed=$(trace_one "$engine" relaxed)
  sync=$(trace_one "$engine" sync)
  verdict=ok
  if (( sync <= relaxed )); then verdict='CHECK'; fi
  printf '%-12s %10d %10d %s\n' "$engine" "$relaxed" "$sync" "$verdict"
  [[ "$verdict" == ok ]] || exit 1
done

# Persy background_sync still fsyncs, just after acknowledgement, so a total
# syscall count cannot distinguish its relaxed and foreground-sync semantics.
relaxed=$(trace_one persy relaxed)
sync=$(trace_one persy sync)
printf '%-12s %10d %10d %s\n' persy "$relaxed" "$sync" informational

for engine in lkv manifold jammdb lsmdb; do
  sync=$(trace_one "$engine" sync)
  verdict=$([[ $sync -gt 0 ]] && echo ok || echo CHECK)
  printf '%-12s %10s %10d %s\n' "$engine" n/a "$sync" "$verdict"
  (( sync > 0 ))
done
