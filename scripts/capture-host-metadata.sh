#!/usr/bin/env bash
set -euo pipefail
OUT=${1:?usage: capture-host-metadata.sh OUTPUT [BENCH_ROOT]}
ROOT=${2:-/srv/scratch/db-bench-2026-09-27}
mkdir -p "$(dirname "$OUT")"
SOURCE=$(findmnt -T "$ROOT" -n -o SOURCE 2>/dev/null || true)
FSTYPE=$(findmnt -T "$ROOT" -n -o FSTYPE 2>/dev/null || true)
DEV=""
if [[ "$SOURCE" == /dev/* ]]; then
  DEV=$(basename "$SOURCE")
  PARENT=$(lsblk -n -o PKNAME "$SOURCE" 2>/dev/null | head -n1 || true)
  [[ -n "$PARENT" ]] && DEV="$PARENT"
fi
{
  echo "captured_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "benchmark_root=$ROOT"
  echo "source=$SOURCE"
  echo "whole_device=$DEV"
  echo
  echo "## kernel"
  uname -a
  cat /proc/version
  echo
  echo "## toolchain"
  RUSTC_REAL=$(rustup which rustc --toolchain 1.99.0 2>/dev/null || true)
  [[ -x "$RUSTC_REAL" ]] && "$RUSTC_REAL" --version --verbose || true
  "$ROOT/scripts/cargo-local-1.99.sh" --version || true
  echo "benchmark_cargo_home=${DB_BENCH_CARGO_HOME:-/tmp/db-bench-cargo-home}"
  cc --version 2>/dev/null | head -n1 || true
  c++ --version 2>/dev/null | head -n1 || true
  uv --version || true
  fio --version || true
  strace --version 2>/dev/null | head -n1 || true
  echo
  echo "## cpu"
  lscpu
  echo "clocksource=$(cat /sys/devices/system/clocksource/clocksource0/current_clocksource 2>/dev/null || true)"
  echo "thp=$(cat /sys/kernel/mm/transparent_hugepage/enabled 2>/dev/null || true)"
  for f in /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor /sys/devices/system/cpu/cpu0/cpufreq/scaling_driver /sys/devices/system/cpu/cpu0/cpufreq/scaling_min_freq /sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq; do
    [[ -r "$f" ]] && echo "[$f]" && cat "$f"
  done
  echo
  echo "## memory"
  free -b
  cat /proc/meminfo
  echo
  echo "## load"
  cat /proc/loadavg
  echo
  echo "## pressure"
  for f in /proc/pressure/cpu /proc/pressure/io /proc/pressure/memory; do
    echo "[$f]"
    cat "$f" 2>/dev/null || true
  done
  echo
  echo "## filesystem"
  df -hT "$ROOT"
  findmnt -T "$ROOT" -o TARGET,SOURCE,FSTYPE,OPTIONS
  stat -f "$ROOT"
  if [[ "$FSTYPE" == zfs ]] && command -v zfs >/dev/null 2>&1; then
    echo
    echo "## zfs dataset"
    zfs get -H -o property,value \
      recordsize,primarycache,secondarycache,sync,compression,atime,logbias,dnodesize,xattr \
      "$SOURCE" 2>/dev/null || true
    zpool status -LP "${SOURCE%%/*}" 2>/dev/null || true
    if [[ -r /proc/spl/kstat/zfs/arcstats ]]; then
      echo "[arc]"
      awk '$1 ~ /^(size|c_min|c_max|hits|misses|compressed_size|uncompressed_size)$/ {print $1 "=" $3}' \
        /proc/spl/kstat/zfs/arcstats
    fi
    [[ -r /sys/module/zfs/parameters/zfs_arc_max ]] && \
      echo "zfs_arc_max=$(cat /sys/module/zfs/parameters/zfs_arc_max)"
  fi
  echo
  echo "## block topology"
  lsblk -b -o NAME,MAJ:MIN,TYPE,SIZE,ROTA,RO,MODEL,SERIAL,FSTYPE,FSAVAIL,FSUSE%,MOUNTPOINTS
  echo
  echo "## block queue"
  if [[ -n "$DEV" && -d "/sys/class/block/$DEV/queue" ]]; then
    for q in rotational scheduler nr_requests read_ahead_kb logical_block_size physical_block_size minimum_io_size optimal_io_size max_sectors_kb nomerges rq_affinity wbt_lat_usec write_cache; do
      [[ -r "/sys/class/block/$DEV/queue/$q" ]] && printf '%s=' "$q" && cat "/sys/class/block/$DEV/queue/$q"
    done
    echo "stat=$(cat "/sys/class/block/$DEV/stat" 2>/dev/null || true)"
  fi
  echo
  echo "## process limits"
  ulimit -a
  echo
  echo "## cgroup"
  cat /proc/self/cgroup
  for f in cpu.max cpu.weight cpuset.cpus.effective memory.current memory.max memory.swap.current memory.swap.max io.max io.weight io.stat; do
    [[ -r "/sys/fs/cgroup/$f" ]] && echo "[$f]" && cat "/sys/fs/cgroup/$f"
  done
  echo
  echo "## vm knobs"
  for f in dirty_background_bytes dirty_background_ratio dirty_bytes dirty_expire_centisecs dirty_ratio dirty_writeback_centisecs swappiness vfs_cache_pressure overcommit_memory overcommit_ratio page-cluster; do
    [[ -r "/proc/sys/vm/$f" ]] && printf '%s=' "$f" && cat "/proc/sys/vm/$f"
  done
  echo
  echo "## vmstat"
  cat /proc/vmstat
  echo
  echo "## diskstats"
  cat /proc/diskstats
  echo
  echo "## provenance"
  sha256sum Cargo.lock Cargo.toml rust-toolchain src/metrics.rs src/bin/*.rs scripts/*.sh scripts/*.py docs/*.md engines/surrealdb-rocksdb/Cargo.toml engines/surrealdb-rocksdb/Cargo.lock engines/surrealdb-rocksdb/src/*.rs 2>/dev/null | sort
  if [[ -x "$ROOT/.deps/sqlite-3.53.4/bin/sqlite3" ]]; then
    echo "sqlite_runtime=$("$ROOT/.deps/sqlite-3.53.4/bin/sqlite3" --version)"
  fi
} > "$OUT"
