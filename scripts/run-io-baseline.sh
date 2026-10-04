#!/usr/bin/env bash
set -euo pipefail
PROFILE=${1:-quick}
case "$PROFILE" in
  smoke) SIZE=256M; RUNTIME=3 ;;
  quick) SIZE=2G; RUNTIME=15 ;;
  full) SIZE=8G; RUNTIME=45 ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac
ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-io-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"; DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR" "$DATA_DIR"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
FILE="$DATA_DIR/fio-baseline.bin"
fio --name=prepare --filename="$FILE" --size="$SIZE" --rw=write --bs=1M --ioengine=psync --direct=1 --fsync_on_close=1 --output-format=json > "$RUN_DIR/prepare.json"

run() {
  local name=$1; shift
  echo "fio $name" >&2
  fio --name="$name" --filename="$FILE" --size="$SIZE" --time_based=1 --runtime="$RUNTIME" \
    --randrepeat=1 --group_reporting=1 --output-format=json "$@" > "$RUN_DIR/$name.json"
}
run direct-seqread-1m-q1 --rw=read --bs=1M --ioengine=psync --direct=1
run direct-seqwrite-1m-q1 --rw=write --bs=1M --ioengine=psync --direct=1
run direct-randread-4k-q1 --rw=randread --bs=4k --ioengine=psync --direct=1
run direct-randwrite-4k-q1 --rw=randwrite --bs=4k --ioengine=psync --direct=1
run direct-randrw70-4k-q1 --rw=randrw --rwmixread=70 --bs=4k --ioengine=psync --direct=1
run direct-randread-4k-q16 --rw=randread --bs=4k --ioengine=libaio --iodepth=16 --direct=1
run direct-randwrite-4k-q16 --rw=randwrite --bs=4k --ioengine=libaio --iodepth=16 --direct=1
run buffered-randread-4k-q1 --rw=randread --bs=4k --ioengine=psync --direct=0
run buffered-seqread-1m-q1 --rw=read --bs=1M --ioengine=psync --direct=0
run fdatasync-write-4k-q1 --rw=write --bs=4k --ioengine=psync --direct=0 --fdatasync=1
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
echo "$RUN_DIR"
