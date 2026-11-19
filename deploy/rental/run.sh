#!/usr/bin/env bash
set -Eeuo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
PROFILE=${RENTAL_PROFILE:-quick}
LANES=${RENTAL_LANES:-kv,record}
RUN_BASE=${RENTAL_RUN_ID:-}
WORK_ROOT=${RENTAL_WORK_ROOT:-/work}
BIN_DIR=${RENTAL_BIN_DIR:-$ROOT/.rental/bin}
MANIFEST=${RENTAL_MANIFEST:-$ROOT/.rental/manifest.json}
PROVIDER=${RENTAL_PROVIDER:-local}
INSTANCE=${RENTAL_INSTANCE_ID:-${VAST_CONTAINERLABEL:-${HOSTNAME:-unknown}}}
PRECHECK=0
DRY_RUN=0

usage() {
  cat <<'USAGE'
Usage: deploy/rental/run.sh [--profile smoke|quick|full] [--lanes kv,record,tsdb]
                            [--run-id UNIQUE_ID] [--work-root ABSOLUTE_PATH]
                            [--preflight-only | --dry-run]
Requires a prepared .rental/manifest.json and its SHA-pinned binaries.
By default results must be durable: provide RENTAL_RESULTS_PUT_URL (presigned PUT),
or explicitly set RENTAL_RESULTS_PERSISTED=1 for an externally mounted volume.
Never run privileged cache-dropping, power-loss, or device-mapper lanes in
an unprivileged Vast/SaladCloud container.
USAGE
}
die() { echo "rental: $*" >&2; exit 2; }
while (($#)); do
  case "$1" in
    --profile|--lanes|--run-id|--work-root)
      (($# >= 2)) || die "missing argument for $1"
      case "$1" in
        --profile) PROFILE=$2 ;;
        --lanes) LANES=$2 ;;
        --run-id) RUN_BASE=$2 ;;
        --work-root) WORK_ROOT=$2 ;;
      esac
      shift 2 ;;
    --preflight-only) PRECHECK=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
case "$PROFILE" in smoke|quick|full) ;; *) die "unsupported profile: $PROFILE" ;; esac
case "$PROVIDER" in vast|salad|local) ;; *) die "unsupported provider: $PROVIDER" ;; esac
[[ "$INSTANCE" =~ ^[a-zA-Z0-9][a-zA-Z0-9_.:-]{0,127}$ ]] || die "invalid instance identity"
[[ "$WORK_ROOT" == /* ]] || die "work root must be absolute"
[[ "$WORK_ROOT" != / && "$WORK_ROOT" != /proc && "$WORK_ROOT" != /sys ]] || die "unsafe work root"
[[ "$RUN_BASE" == "" || "$RUN_BASE" =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]{0,79}$ ]] || die "invalid run ID"
[[ "$LANES" =~ ^(kv|record|tsdb)(,(kv|record|tsdb))*$ ]] || die "malformed or unsupported lane set"
IFS=, read -r -a SELECTED <<< "$LANES"
declare -A SEEN=()
for lane in "${SELECTED[@]}"; do
  case "$lane" in kv|record|tsdb) ;; *) die "unsupported lane: $lane" ;; esac
  [[ -z "${SEEN[$lane]:-}" ]] || die "duplicate lane: $lane"
  SEEN[$lane]=1
done
for tool in jq cmp sha256sum findmnt df timeout tar zstd date hostname awk; do
  command -v "$tool" >/dev/null 2>&1 || die "missing required runtime tool: $tool"
done
[[ -r "$MANIFEST" ]] || die "missing rental manifest: $MANIFEST"
ISA=$(jq -er '.compile_target' "$MANIFEST") || die "missing compiled ISA target"
[[ "$ISA" == "x86-64-v2+aes" ]] || die "unsupported compiled ISA target: $ISA"
cpu_flags=$(awk '/^flags[[:space:]]*:/ {sub(/^[^:]*:[[:space:]]*/, "");print;exit}' /proc/cpuinfo)
for feature in aes pni ssse3 sse4_1 sse4_2 popcnt cx16 lahf_lm; do
  [[ " $cpu_flags " == *" $feature "* ]] || die "rental CPU lacks required x86-64-v2+aes feature: $feature"
done
SOURCE_SHA=$(jq -er '.source_commit | select(type=="string" and test("^[0-9a-f]{40}$"))' "$MANIFEST") || die "invalid manifest source commit"
if [[ -n "${SEEN[record]:-}" ]]; then
  jq -e '.record_sqlite_version == "3.54.0" and .record_sqlite_linkage == "static"' "$MANIFEST" >/dev/null || die "record image lacks pinned static SQLite 3.54.0 identity"
fi
CURRENT_SHA=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null) || die "source bundle must preserve its pinned git checkout"
[[ "$SOURCE_SHA" == "$CURRENT_SHA" ]] || die "source commit differs from manifest: $CURRENT_SHA != $SOURCE_SHA"
[[ -z "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ]] || die "tracked source tree is modified"
required=(all-db-benchctl)
for lane in "${SELECTED[@]}"; do
  case "$lane" in
    kv) required+=(kvbench) ;;
    record) required+=(recordbench surrealdb-rocksdb-recordbench) ;;
    tsdb) required+=(tsdb-server-bench) ;;
  esac
done
for name in "${required[@]}"; do
  file="$BIN_DIR/$name"
  [[ -x "$file" ]] || die "missing executable: $file"
  expected=$(jq -er --arg name "$name" '.binaries[$name] | select(type=="string" and test("^[0-9a-f]{64}$"))' "$MANIFEST") || die "missing SHA-256 for $name"
  actual=$(sha256sum "$file" | awk '{print $1}')
  [[ "$actual" == "$expected" ]] || die "binary $name differs from manifest"
  if command -v ldd >/dev/null 2>&1; then
    linkage=$(ldd "$file" 2>&1 || true)
    if [[ "$linkage" == *"not found"* || "$linkage" == *"version \`GLIBC_"* ]]; then
      echo "$linkage" >&2
      die "$name cannot load with the rental image libraries"
    fi
    if [[ "$name" == recordbench && "$linkage" == *libsqlite3* ]]; then
      die "recordbench is dynamically linked to libsqlite3; pinned static linkage required"
    fi
  fi
done
if [[ -n "${SEEN[tsdb]:-}" ]]; then
  for path in "$ROOT"/.deps/tsdb/greptimedb-*/greptime "$ROOT"/.deps/tsdb/victoriametrics-*/victoria-metrics-prod "$ROOT"/.deps/tsdb/prometheus-*/prometheus "$ROOT"/.deps/tsdb/influxdb3-*/influxdb3; do
    [[ -x "$path" ]] || die "TSDB binary not staged: $path"
  done
fi
mkdir -p "$WORK_ROOT/data" "$WORK_ROOT/results"
[[ -w "$WORK_ROOT/data" && -w "$WORK_ROOT/results" ]] || die "work directories are not writable"
if [[ "$PRECHECK" == 0 && "$DRY_RUN" == 0 && -z "${RENTAL_RESULTS_PUT_URL:-}" && "${RENTAL_RESULTS_PERSISTED:-0}" != 1 ]]; then
  die "no durable result destination: use RENTAL_RESULTS_PUT_URL or RENTAL_RESULTS_PERSISTED=1"
fi
[[ -z "${RENTAL_RESULTS_PUT_URL:-}" ]] || command -v curl >/dev/null || die "curl required for result upload"

# A rental's effective identity is not the image-global /etc/machine-id.
export DBBENCH_MACHINE_ID_SHA256
DBBENCH_MACHINE_ID_SHA256=$(printf 'rental-v1\0%s\0%s' "$PROVIDER" "$INSTANCE" | sha256sum | awk '{print $1}')
export DBBENCH_STORAGE_ROOT="$WORK_ROOT"
export BENCHCTL_BIN="$BIN_DIR/all-db-benchctl"
export CONTINUOUS_NOISE_BENCHCTL="$BENCHCTL_BIN"
export CONTINUOUS_NOISE_BENCHCTL_READY=1

available=$(df -B1 --output=avail "$WORK_ROOT/data" | tail -n1 | tr -d ' ')
case "$PROFILE" in smoke) min_disk=2; min_mem=1 ;; quick) min_disk=10; min_mem=4 ;; full) min_disk=30; min_mem=8 ;; esac
(( available >= min_disk * 1024 * 1024 * 1024 )) || die "insufficient available data disk: needs ${min_disk} GiB"
memory_max=$(cat /sys/fs/cgroup/memory.max 2>/dev/null || printf max)
if [[ "$memory_max" =~ ^[0-9]+$ ]] && (( memory_max < min_mem * 1024 * 1024 * 1024 )); then
  die "cgroup memory.max below the ${min_mem} GiB $PROFILE baseline"
fi
printf 'rental preflight: source=%s provider=%s instance=%s profile=%s lanes=%s free_GiB=%s memory.max=%s\n' \
  "$SOURCE_SHA" "$PROVIDER" "$INSTANCE" "$PROFILE" "$LANES" "$((available/1024/1024/1024))" "$memory_max"
((PRECHECK == 0)) || exit 0

[[ -n "$RUN_BASE" ]] || RUN_BASE="$(date -u +%Y%m%dT%H%M%SZ)-$(cat /proc/sys/kernel/random/uuid | cut -c1-8)"
STATE_DIR="$WORK_ROOT/results/rental/$RUN_BASE"
mkdir -p "$STATE_DIR"
STATE="$STATE_DIR/status.json"
STARTED=$(date -u +%Y-%m-%dT%H:%M:%SZ)
DEADLINE=${RENTAL_DEADLINE_UNIX:-0}
[[ "$DEADLINE" =~ ^[0-9]+$ ]] || die "RENTAL_DEADLINE_UNIX must be a Unix seconds timestamp"
if ((DEADLINE == 0)); then
  max_seconds=${RENTAL_MAX_SECONDS:-28800}
  [[ "$max_seconds" =~ ^[0-9]+$ ]] && ((max_seconds > 0)) || die "RENTAL_MAX_SECONDS must be positive"
  DEADLINE=$(( $(date +%s) + max_seconds ))
fi
write_status() {
  local phase=$1 lane=$2 rc=$3 now tmp
  now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  tmp="$STATE.new.$$"
  jq -n --arg phase "$phase" --arg lane "$lane" --arg started "$STARTED" --arg updated "$now" \
    --arg provider "$PROVIDER" --arg instance "$INSTANCE" --arg source "$SOURCE_SHA" \
    --arg profile "$PROFILE" --arg lanes "$LANES" --arg run_id "$RUN_BASE" --argjson deadline "$DEADLINE" --argjson rc "$rc" \
    '{version:1,run_id:$run_id,phase:$phase,current_lane:$lane,exit_code:$rc,started_utc:$started,updated_utc:$updated,deadline_unix:$deadline,provider:$provider,instance_id:$instance,source_commit:$source,profile:$profile,lanes:$lanes}' > "$tmp"
  mv -f "$tmp" "$STATE"
}
upload_results() {
  [[ -n "${RENTAL_RESULTS_PUT_URL:-}" ]] || return 0
  local temp archive
  temp=$(mktemp -d "$WORK_ROOT/.rental-upload.XXXXXXXX")
  archive="$temp/results.tar.zst"
  # Export only this rental's results. A reused persistent volume can contain
  # unrelated historical runs and must never leak them into this object.
  local -a paths=("results/rental/$RUN_BASE")
  local lane path
  for lane in "${SELECTED[@]}"; do
    path="results/runs/$RUN_BASE-$lane-$PROFILE"
    [[ ! -d "$WORK_ROOT/$path" ]] || paths+=("$path")
  done
  tar -C "$WORK_ROOT" --exclude='*.new.*' -cf - "${paths[@]}" | zstd -q -T1 -3 -o "$archive" || { rm -rf -- "$temp"; return 1; }
  local rc=0
  curl --fail --silent --show-error --retry 3 --retry-all-errors --connect-timeout 15 --max-time 900 \
    --request PUT --upload-file "$archive" "$RENTAL_RESULTS_PUT_URL" --output /dev/null || rc=$?
  rm -rf -- "$temp"
  return "$rc"
}
CURRENT_LANE=none
ACTIVE_CHILD=0
on_signal() {
  local code=$1
  trap - TERM INT
  if (( ACTIVE_CHILD > 0 )); then
    kill -TERM "$ACTIVE_CHILD" 2>/dev/null || true
    wait "$ACTIVE_CHILD" 2>/dev/null || true
  fi
  exit "$code"
}
on_exit() {
  local rc=$1
  trap - EXIT
  set +e
  write_status "$([[ "$rc" == 0 ]] && echo complete || echo interrupted)" "$CURRENT_LANE" "$rc"
  upload_results
  local upload_rc=$?
  if ((upload_rc != 0)); then echo "rental: results upload failed, local checkpoint retained" >&2; ((rc != 0)) || rc=74; fi
  exit "$rc"
}
trap 'on_exit $?' EXIT
trap 'on_signal 143' TERM
trap 'on_signal 130' INT
write_status starting none 0
if ((DRY_RUN)); then
  for lane in "${SELECTED[@]}"; do
    printf 'would run RUN_ID=%s-%s-%s %s %s\n' "$RUN_BASE" "$lane" "$PROFILE" "$ROOT/scripts/run-$lane-matrix.sh" "$PROFILE"
  done
  exit 0
fi
if [[ "$ROOT/results" != "$WORK_ROOT/results" ]]; then
  [[ "$(realpath -m "$ROOT/results")" == "$(realpath -m "$WORK_ROOT/results")" ]] || \
    die "results must resolve to $WORK_ROOT/results (bind mount or symlink ROOT/results)"
fi
for lane in "${SELECTED[@]}"; do
  CURRENT_LANE=$lane
  now=$(date +%s)
  (( DEADLINE > now )) || { echo "rental: deadline reached before $lane" >&2; exit 124; }
  write_status running "$lane" 0
  case "$lane" in
    kv) export BENCH_BIN="$BIN_DIR/kvbench"; unset ROCKS_BENCH_BIN || true ;;
    record) export BENCH_BIN="$BIN_DIR/recordbench" ROCKS_BENCH_BIN="$BIN_DIR/surrealdb-rocksdb-recordbench" DBBENCH_PINNED_SQLITE_PREBUILT=1 ;;
    tsdb) export TSDB_BENCH_BIN="$BIN_DIR/tsdb-server-bench"; unset BENCH_BIN ROCKS_BENCH_BIN || true ;;
  esac
  export RUN_ID="$RUN_BASE-$lane-$PROFILE"
  timeout --signal=TERM --kill-after=30s "$((DEADLINE-now))" "$ROOT/scripts/run-$lane-matrix.sh" "$PROFILE" &
  ACTIVE_CHILD=$!
  wait "$ACTIVE_CHILD"
  ACTIVE_CHILD=0
  write_status checkpoint "$lane" 0
  upload_results
done
