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
for tool in fio jq findmnt sha256sum uv awk; do
  command -v "$tool" >/dev/null 2>&1 || { echo "required tool not found: $tool" >&2; exit 127; }
done
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
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR" "$DATA_DIR"
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
if (( free_bytes < min_free_bytes )); then
  echo "refusing storage calibration: free=$free_bytes required=$min_free_bytes" >&2
  exit 75
fi

HOST_NAME=$(hostname)
MACHINE_ID=$(cat /etc/machine-id 2>/dev/null || printf unknown)
MOUNT_TARGET=$(findmnt -T "$DATA_DIR" -n -o TARGET)
MOUNT_SOURCE=$(findmnt -T "$DATA_DIR" -n -o SOURCE)
MOUNT_FSTYPE=$(findmnt -T "$DATA_DIR" -n -o FSTYPE)
MOUNT_OPTIONS=$(findmnt -T "$DATA_DIR" -n -o OPTIONS)
STAT_DEVICE=$(stat -c '%d' "$DATA_DIR")
FIO_VERSION=$(fio --version)
GIT_COMMIT=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || printf unknown)

"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
FILE="$DATA_DIR/fio-baseline.bin"
cleanup() { rm -f -- "$FILE"; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fio --name=prepare --filename="$FILE" --size="$SIZE" --rw=write --bs=1M \
  --ioengine=psync --direct=1 --fsync_on_close=1 --output-format=json > "$RUN_DIR/prepare.json"

run() {
  local name=$1
  shift
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
uv run --script "$ROOT/scripts/summarize-io.py" "$RUN_DIR" \
  --json-out "$RUN_DIR/io-summary.json" --markdown-out "$RUN_DIR/io-summary.md"

BASELINE_JSON="$RUN_DIR/direct-randrw70-4k-q1.json"
BASELINE_SHA=$(sha256sum "$BASELINE_JSON" | awk '{print $1}')
BASE_IOPS=$(jq -r '.calibration.randrw70_4k_q1_iops' "$RUN_DIR/io-summary.json")
jq -n \
  --arg run_id "$RUN_ID" \
  --arg profile "$PROFILE" \
  --arg size "$SIZE" \
  --argjson runtime_s "$RUNTIME" \
  --arg hostname "$HOST_NAME" \
  --arg machine_id "$MACHINE_ID" \
  --arg mount_target "$MOUNT_TARGET" \
  --arg mount_source "$MOUNT_SOURCE" \
  --arg filesystem "$MOUNT_FSTYPE" \
  --arg mount_options "$MOUNT_OPTIONS" \
  --arg stat_device "$STAT_DEVICE" \
  --arg fio_version "$FIO_VERSION" \
  --arg git_commit "$GIT_COMMIT" \
  --arg baseline_sha256 "$BASELINE_SHA" \
  --argjson baseline_iops "$BASE_IOPS" \
  '{format_version:1,lane:"io-calibration",complete:true,run_id:$run_id,profile:$profile,file_size:$size,runtime_s:$runtime_s,hostname:$hostname,machine_id:$machine_id,storage:{mount_target:$mount_target,mount_source:$mount_source,filesystem:$filesystem,mount_options:$mount_options,stat_device:$stat_device},fio_version:$fio_version,git_commit:$git_commit,randrw70_4k_q1:{sha256:$baseline_sha256,iops:$baseline_iops}}' \
  > "$RUN_DIR/calibration.json"

echo "$RUN_DIR"
