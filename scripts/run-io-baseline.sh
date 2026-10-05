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
mkdir -p "$RUN_DIR/storage" "$DATA_DIR"

free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
if (( free_bytes < min_free_bytes )); then
  echo "refusing storage calibration: free=$free_bytes required=$min_free_bytes" >&2
  exit 75
fi

FSTYPE=$(findmnt -T "$DATA_DIR" -n -o FSTYPE 2>/dev/null || true)
SOURCE=$(findmnt -T "$DATA_DIR" -n -o SOURCE 2>/dev/null || true)
if [[ "$FSTYPE" == zfs ]]; then
  command -v zfs >/dev/null 2>&1 || { echo "required tool not found for ZFS calibration: zfs" >&2; exit 127; }
fi
PRESSURE_BS_BYTES=4096
PREP_BS_BYTES=1048576
ZFS_RECORDSIZE_BYTES=null
ZFS_DIRECT_MODE=""
ZFS_DIO_ENABLED=""
SMALL_WRITE_DIRECT_STATUS="direct-requested"
PRESSURE_REASON="4 KiB direct mixed I/O is used on non-ZFS filesystems"

if [[ "$FSTYPE" == zfs ]]; then
  ZFS_RECORDSIZE_BYTES=$(zfs get -Hp -o value recordsize "$SOURCE" 2>/dev/null || true)
  ZFS_DIRECT_MODE=$(zfs get -H -o value direct "$SOURCE" 2>/dev/null || true)
  ZFS_DIO_ENABLED=$(cat /sys/module/zfs/parameters/zfs_dio_enabled 2>/dev/null || true)
  if [[ ! "$ZFS_RECORDSIZE_BYTES" =~ ^[0-9]+$ ]] || (( ZFS_RECORDSIZE_BYTES < 4096 )); then
    echo "cannot determine usable ZFS recordsize for direct-I/O calibration: source=$SOURCE recordsize=$ZFS_RECORDSIZE_BYTES" >&2
    exit 75
  fi
  if [[ "$ZFS_DIRECT_MODE" == disabled || "$ZFS_DIO_ENABLED" == 0 ]]; then
    echo "refusing raw direct-I/O calibration: ZFS direct I/O is disabled (dataset direct=$ZFS_DIRECT_MODE zfs_dio_enabled=$ZFS_DIO_ENABLED)" >&2
    exit 75
  fi
  PRESSURE_BS_BYTES=$ZFS_RECORDSIZE_BYTES
  if (( PRESSURE_BS_BYTES > PREP_BS_BYTES )); then PREP_BS_BYTES=$PRESSURE_BS_BYTES; fi
  PRESSURE_REASON="ZFS direct writes require recordsize-aligned offset and length; pressure calibration therefore uses the dataset recordsize"
  if (( ZFS_RECORDSIZE_BYTES > 4096 )); then
    SMALL_WRITE_DIRECT_STATUS="requested O_DIRECT; 4 KiB writes are not recordsize-aligned and ZFS redirects them through ARC"
  else
    SMALL_WRITE_DIRECT_STATUS="direct-eligible because 4 KiB equals the ZFS recordsize"
  fi
fi

"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT" || exit $?
HOST_NAME=$(hostname)
MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FIO_VERSION=$(fio --version)
GIT_COMMIT=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || printf unknown)
FILE="$DATA_DIR/fio-baseline.bin"
cleanup() { rm -f -- "$FILE"; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

jq -n \
  --arg lane "io-baseline" \
  --arg profile "$PROFILE" \
  --arg host_name "$HOST_NAME" \
  --arg machine_id_sha256 "$MACHINE_ID_SHA256" \
  --arg fio_version "$FIO_VERSION" \
  --arg git_commit "$GIT_COMMIT" \
  --arg filesystem "$FSTYPE" \
  --arg source "$SOURCE" \
  --arg zfs_direct_mode "$ZFS_DIRECT_MODE" \
  --arg zfs_dio_enabled "$ZFS_DIO_ENABLED" \
  --arg small_write_direct_status "$SMALL_WRITE_DIRECT_STATUS" \
  --arg pressure_reason "$PRESSURE_REASON" \
  --arg pressure_file "pressure-calibration-randrw70-q1.json" \
  --argjson pressure_bs_bytes "$PRESSURE_BS_BYTES" \
  --argjson zfs_recordsize_bytes "$ZFS_RECORDSIZE_BYTES" \
  '{
    calibration_protocol_version:3,
    lane:$lane,
    profile:$profile,
    host_name:$host_name,
    machine_id_sha256:$machine_id_sha256,
    fio_version:$fio_version,
    git_commit:$git_commit,
    filesystem:$filesystem,
    source:$source,
    zfs_recordsize_bytes:$zfs_recordsize_bytes,
    zfs_direct_mode:$zfs_direct_mode,
    zfs_dio_enabled:$zfs_dio_enabled,
    requested_direct_4k_write_semantics:$small_write_direct_status,
    tests:[
      "direct-seqread-1m-q1.json",
      "direct-seqwrite-1m-q1.json",
      "direct-randread-4k-q1.json",
      "direct-randwrite-4k-q1.json",
      "direct-randrw70-4k-q1.json",
      "pressure-calibration-randrw70-q1.json",
      "direct-randread-4k-q16.json",
      "direct-randwrite-4k-q16.json",
      "buffered-randread-4k-q1.json",
      "buffered-seqread-1m-q1.json",
      "fdatasync-write-4k-q1.json"
    ],
    pressure_calibration:{
      file:$pressure_file,
      bs_bytes:$pressure_bs_bytes,
      rw:"randrw",
      rwmixread:70,
      ioengine:"psync",
      iodepth:1,
      direct:1,
      blockalign_bytes:$pressure_bs_bytes,
      reason:$pressure_reason
    },
    interpretation:"Tests named direct record fio O_DIRECT requests. On ZFS, unaligned direct writes may be redirected through ARC; support metadata records that distinction. The pressure-calibration workload is chosen so writes are direct-eligible."
  }' > "$RUN_DIR/support.json"

fio --name=prepare --filename="$FILE" --size="$SIZE" --rw=write --bs="$PREP_BS_BYTES" \
  --ioengine=psync --direct=1 --fsync_on_close=1 --output-format=json > "$RUN_DIR/prepare.json" || exit $?

run() {
  local name=$1; shift
  echo "fio $name" >&2
  local storage_before storage_after
  storage_before=$("$ROOT/scripts/capture-storage-counters.sh" "$DATA_DIR")
  set +e
  fio --name="$name" --filename="$FILE" --size="$SIZE" --time_based=1 --runtime="$RUNTIME" \
    --randrepeat=1 --group_reporting=1 --output-format=json "$@" > "$RUN_DIR/$name.json"
  local rc=$?
  set -e
  storage_after=$("$ROOT/scripts/capture-storage-counters.sh" "$DATA_DIR")
  jq -cn --argjson before "$storage_before" --argjson after "$storage_after" \
    '{before:$before,after:$after}' > "$RUN_DIR/storage/$name.json"
  return "$rc"
}

run direct-seqread-1m-q1 --rw=read --bs=1M --ioengine=psync --direct=1 || exit $?
run direct-seqwrite-1m-q1 --rw=write --bs=1M --ioengine=psync --direct=1 || exit $?
run direct-randread-4k-q1 --rw=randread --bs=4k --ioengine=psync --direct=1 || exit $?
run direct-randwrite-4k-q1 --rw=randwrite --bs=4k --ioengine=psync --direct=1 || exit $?
run direct-randrw70-4k-q1 --rw=randrw --rwmixread=70 --bs=4k --ioengine=psync --direct=1 || exit $?
run pressure-calibration-randrw70-q1 --rw=randrw --rwmixread=70 --bs="$PRESSURE_BS_BYTES" --blockalign="$PRESSURE_BS_BYTES" --ioengine=psync --direct=1 || exit $?
CALIBRATION_JSON="$RUN_DIR/pressure-calibration-randrw70-q1.json"
if ! jq -e --arg bs "$PRESSURE_BS_BYTES" '
  (.jobs | length) == 1 and
  (.jobs[0].error // 0) == 0 and
  .jobs[0]["job options"].rw == "randrw" and
  .jobs[0]["job options"].rwmixread == "70" and
  .jobs[0]["job options"].bs == $bs and
  .jobs[0]["job options"].ba == $bs and
  .jobs[0]["job options"].ioengine == "psync" and
  .jobs[0]["job options"].direct == "1"
' "$CALIBRATION_JSON" >/dev/null; then
  echo "pressure calibration fio JSON does not match the protocol-v3 aligned direct-I/O contract" >&2
  exit 1
fi
run direct-randread-4k-q16 --rw=randread --bs=4k --ioengine=libaio --iodepth=16 --direct=1 || exit $?
run direct-randwrite-4k-q16 --rw=randwrite --bs=4k --ioengine=libaio --iodepth=16 --direct=1 || exit $?
run buffered-randread-4k-q1 --rw=randread --bs=4k --ioengine=psync --direct=0 || exit $?
run buffered-seqread-1m-q1 --rw=read --bs=1M --ioengine=psync --direct=0 || exit $?
run fdatasync-write-4k-q1 --rw=write --bs=4k --ioengine=psync --direct=0 --fdatasync=1 || exit $?

"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT" || exit $?
uv run --script "$ROOT/scripts/summarize-io-baseline.py" "$RUN_DIR" \
  --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md" || exit $?

BASELINE_SHA=$(sha256sum "$CALIBRATION_JSON" | awk '{print $1}')
SUPPORT_SHA=$(sha256sum "$RUN_DIR/support.json" | awk '{print $1}')
SUMMARY_SHA=$(sha256sum "$RUN_DIR/summary.json" | awk '{print $1}')
BASE_IOPS=$(jq -r '(.jobs[0].read.iops // 0) + (.jobs[0].write.iops // 0)' "$CALIBRATION_JSON")
jq -n \
  --arg run_id "$RUN_ID" \
  --arg profile "$PROFILE" \
  --arg hostname "$HOST_NAME" \
  --arg machine_id_sha256 "$MACHINE_ID_SHA256" \
  --arg filesystem "$FSTYPE" \
  --arg source "$SOURCE" \
  --arg fio_version "$FIO_VERSION" \
  --arg git_commit "$GIT_COMMIT" \
  --arg pressure_file "$(basename "$CALIBRATION_JSON")" \
  --arg pressure_sha256 "$BASELINE_SHA" \
  --arg support_sha256 "$SUPPORT_SHA" \
  --arg summary_sha256 "$SUMMARY_SHA" \
  --argjson pressure_bs_bytes "$PRESSURE_BS_BYTES" \
  --argjson baseline_iops "$BASE_IOPS" \
  '{format_version:1,lane:"io-calibration",complete:true,calibration_protocol_version:3,
    run_id:$run_id,profile:$profile,hostname:$hostname,machine_id_sha256:$machine_id_sha256,
    storage:{filesystem:$filesystem,source:$source},fio_version:$fio_version,git_commit:$git_commit,
    pressure_calibration:{file:$pressure_file,sha256:$pressure_sha256,bs_bytes:$pressure_bs_bytes,iops:$baseline_iops},
    support_sha256:$support_sha256,summary_sha256:$summary_sha256}' > "$RUN_DIR/calibration.json"

echo "$RUN_DIR"
