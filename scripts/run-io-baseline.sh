#!/usr/bin/env bash
set -euo pipefail
PROFILE=${1:-quick}
case "$PROFILE" in
  smoke) SIZE=256M; RUNTIME=3; MIN_FREE_GIB=2 ;;
  quick) SIZE=2G; RUNTIME=15; MIN_FREE_GIB=10 ;;
  full) SIZE=8G; RUNTIME=45; MIN_FREE_GIB=30 ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac
ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
CPUS=$(getconf _NPROCESSORS_ONLN)
LOAD1=$(awk '{print $1}' /proc/loadavg)
IO_PSI10=$(awk '/^full / {for(i=1;i<=NF;i++) if($i ~ /^avg10=/){split($i,a,"="); print a[2]}}' /proc/pressure/io)
if [[ "${ALLOW_BUSY:-0}" != 1 ]]; then
  if ! awk -v l="$LOAD1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 0.5) }'; then
    echo "refusing storage calibration on busy host: load1=$LOAD1 cpus=$CPUS" >&2
    exit 75
  fi
  if ! awk -v p="${IO_PSI10:-0}" 'BEGIN { exit !(p <= 5.0) }'; then
    echo "refusing storage calibration under existing I/O pressure: io PSI full avg10=${IO_PSI10}%" >&2
    exit 75
  fi
fi
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-io-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"; DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR" "$DATA_DIR"
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
if (( free_bytes < min_free_bytes )); then
  echo "refusing storage calibration: free=$free_bytes required=$min_free_bytes" >&2
  exit 75
fi
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
FILE="$DATA_DIR/fio-baseline.bin"
cleanup() { rm -f -- "$FILE"; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
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
