#!/usr/bin/env bash
set -euo pipefail
ROOT=${1:-/srv/scratch/db-bench-2026-09-27}
for tool in findmnt jq awk readlink lsblk; do
  command -v "$tool" >/dev/null 2>&1 || { echo "required tool not found: $tool" >&2; exit 127; }
done
FSTYPE=$(findmnt -T "$ROOT" -n -o FSTYPE 2>/dev/null || true)
SOURCE=$(findmnt -T "$ROOT" -n -o SOURCE 2>/dev/null || true)
TMP=$(mktemp)
trap 'rm -f "$TMP"' EXIT

if [[ "$FSTYPE" == zfs ]]; then
  command -v zpool >/dev/null 2>&1 || { echo "required tool not found for ZFS storage counters: zpool" >&2; exit 127; }
  POOL=${SOURCE%%/*}
  zpool status -LP "$POOL" 2>/dev/null | awk '$1 ~ /^\/dev\// {print $1}' | sort -u > "$TMP"
elif [[ "$SOURCE" == /dev/* ]]; then
  printf '%s\n' "$SOURCE" > "$TMP"
fi

DEVICES='[]'
while IFS= read -r path; do
  [[ -n "$path" ]] || continue
  real=$(readlink -f "$path" 2>/dev/null || printf '%s' "$path")
  name=$(basename "$real")
  stat_file="/sys/class/block/$name/stat"
  [[ -r "$stat_file" ]] || continue
  stat_json=$(awk '{printf "["; for(i=1;i<=NF;i++){if(i>1)printf ","; printf "%s",$i}; print "]"}' "$stat_file")
  queue_name="$name"
  if [[ ! -r "/sys/class/block/$queue_name/queue/logical_block_size" ]]; then
    parent=$(lsblk -n -o PKNAME "$real" 2>/dev/null | head -n1 || true)
    [[ -n "$parent" ]] && queue_name="$parent"
  fi
  logical_block_size=$(cat "/sys/class/block/$queue_name/queue/logical_block_size" 2>/dev/null || echo 0)
  physical_block_size=$(cat "/sys/class/block/$queue_name/queue/physical_block_size" 2>/dev/null || echo 0)
  DEVICES=$(jq -cn \
    --argjson devices "$DEVICES" --arg path "$real" --arg name "$name" \
    --argjson stat "$stat_json" --argjson logical "$logical_block_size" \
    --argjson physical "$physical_block_size" \
    '$devices + [{path:$path,name:$name,stat:$stat,logical_block_size:$logical,physical_block_size:$physical}]')
done < "$TMP"

ZFS_POOL_IO='null'
if [[ "$FSTYPE" == zfs ]]; then
  POOL=${SOURCE%%/*}
  POOL_IOSTATS="/proc/spl/kstat/zfs/$POOL/iostats"
  if [[ -r "$POOL_IOSTATS" ]]; then
    ZFS_POOL_IO=$(awk '$1 ~ /^(arc_read_count|arc_read_bytes|arc_write_count|arc_write_bytes|direct_read_count|direct_read_bytes|direct_write_count|direct_write_bytes)$/ {print $1 "=" $3}' \
      "$POOL_IOSTATS" | jq -Rn '[inputs | split("=") | {(.[0]): (.[1]|tonumber)}] | add // {}')
  fi
fi

ARC='null'
if [[ "$FSTYPE" == zfs && -r /proc/spl/kstat/zfs/arcstats ]]; then
  ARC=$(awk '$1 ~ /^(hits|misses|read_iohits|demand_data_hits|demand_data_misses|prefetch_data_hits|prefetch_data_misses|size|compressed_size|uncompressed_size)$/ {print $1 "=" $3}' \
    /proc/spl/kstat/zfs/arcstats | jq -Rn '[inputs | split("=") | {(.[0]): (.[1]|tonumber)}] | add // {}')
fi

jq -cn \
  --argjson captured_monotonic_ns "$(awk '{printf "%.0f", $1*1000000000}' /proc/uptime)" \
  --arg filesystem "$FSTYPE" --arg source "$SOURCE" \
  --argjson devices "$DEVICES" --argjson arc "$ARC" --argjson zfs_pool_io "$ZFS_POOL_IO" \
  '{captured_monotonic_ns:$captured_monotonic_ns,filesystem:$filesystem,source:$source,devices:$devices,zfs_arc:$arc,zfs_pool_io:$zfs_pool_io}'
