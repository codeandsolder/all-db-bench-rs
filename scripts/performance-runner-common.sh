#!/usr/bin/env bash

COMMON_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=continuous-noise-runner-common.sh
source "$COMMON_DIR/continuous-noise-runner-common.sh"
PERFORMANCE_ADMISSION_POLICY=${PERFORMANCE_ADMISSION_POLICY:-$CONTINUOUS_ADMISSION_POLICY}

performance_check_free_space() {
  local profile=$1 phase=$2 path=${3:-${PERFORMANCE_FREE_SPACE_PATH:-${DATA_DIR:-$PWD}}}
  [[ "$profile" == smoke ]] && return 0
  local min_gib=${CASE_MIN_FREE_GIB:-${PERFORMANCE_MIN_FREE_GIB:-${MIN_FREE_GIB:-8}}} free_kib min_kib
  free_kib=$(df -Pk -- "$path" | awk 'NR==2 {print $4}') || return 2
  min_kib=$(awk -v g="$min_gib" 'BEGIN { if (g < 0) exit 2; printf "%.0f", g * 1024 * 1024 }') || return 2
  if (( free_kib < min_kib )); then
    echo "refusing performance case under low free space ($phase): free_kib=$free_kib required_kib=$min_kib path=$path" >&2
    return 75
  fi
}

# The same physical-device gate is used by every measured benchmark family.
# All backing devices are sampled in one interval; each must be idle.
io_admission_check() {
  local phase=$1 threshold=${2:-5.0}
  local json=${PERFORMANCE_IO_QUIET_DEVICES_JSON:-[]}
  if ! jq -e 'type=="array" and all(.[];type=="string" and startswith("/"))' <<<"$json" >/dev/null; then
    echo "invalid PERFORMANCE_IO_QUIET_DEVICES_JSON" >&2; return 2
  fi
  local -a devices=() paths=() reads=() writes=() busy=() counters=()
  mapfile -t devices < <(jq -r '.[]' <<<"$json")
  if ((${#devices[@]})); then
    local seconds=${PERFORMANCE_IO_QUIET_SAMPLE_SECONDS:-1}
    local max_iops=${PERFORMANCE_IO_QUIET_MAX_IOPS:-100}
    local max_busy=${PERFORMANCE_IO_QUIET_MAX_BUSY_PERCENT:-10}
    local number_re='^[0-9]+([.][0-9]+)?$'
    if [[ ! $seconds =~ $number_re || ! $max_iops =~ $number_re || ! $max_busy =~ $number_re ]] ||
      ! awk -v s="$seconds" 'BEGIN {exit !(s>0)}'; then
      echo "invalid physical-device admission limits" >&2; return 2
    fi
    local dev resolved stat i operations busy_ms
    for dev in "${devices[@]}"; do
      resolved=$(realpath -e -- "$dev") || { echo "unresolvable I/O device: $dev" >&2; return 2; }
      [[ -b "$resolved" ]] || { echo "not a block device: $dev" >&2; return 2; }
      stat="/sys/class/block/${resolved##*/}/stat"
      [[ -r "$stat" ]] || { echo "device stat not readable: $stat" >&2; return 2; }
      read -r -a counters <"$stat" || return 2
      ((${#counters[@]} >= 11)) || { echo "short device statistics: $stat" >&2; return 2; }
      paths+=("$stat")
      reads+=("${counters[0]}")
      writes+=("${counters[4]}")
      busy+=("${counters[9]}")
    done
    sleep "$seconds" || return 2
    for i in "${!paths[@]}"; do
      read -r -a counters <"${paths[i]}" || return 2
      ((${#counters[@]} >= 11)) || return 2
      operations=$((counters[0]-reads[i]+counters[4]-writes[i]))
      busy_ms=$((counters[9]-busy[i]))
      if ((operations < 0 || busy_ms < 0)); then
        echo "device I/O statistics reset: ${devices[i]}" >&2; return 2
      fi
      if ! awk -v o="$operations" -v b="$busy_ms" -v s="$seconds" -v i="$max_iops" -v m="$max_busy" \
          'BEGIN {exit !(o/s <= i && b/(s*10) <= m)}'; then
        echo "refusing benchmark under backing-device I/O ($phase): ${devices[i]} operations=$operations busy_ms=$busy_ms interval=$seconds" >&2
        return 75
      fi
    done
    return 0
  fi
  # CPU/memory bounding is not physical-I/O isolation. Fall back to host
  # PSI if a versioned profile does not explicitly bind backing devices.
  local pressure
  pressure=$(awk '/^full / {for(i=1;i<=NF;i++) if($i ~ /^avg10=/){split($i,a,"="); print a[2]}}' /proc/pressure/io) || return 2
  if ! awk -v p="${pressure:-0}" -v t="$threshold" 'BEGIN {exit !(p<=t)}'; then
    echo "refusing benchmark under I/O pressure ($phase): ${pressure}%" >&2
    return 75
  fi
}

performance_check_io_quiet() {
  local profile=$1 phase=$2 threshold=${3:-5.0}
  [[ "$profile" == smoke ]] && return 0
  performance_check_free_space "$profile" "$phase" || return $?
  [[ "${ALLOW_BUSY:-0}" == 1 ]] && return 0

  io_admission_check "$phase" "$threshold"
}

performance_check_external_noise() {
  local root=$1 profile=$2 phase=$3 evidence=$4 rc
  [[ "$profile" == smoke || "${ALLOW_EXTERNAL_NOISE:-0}" == 1 ]] && return 0
  continuous_noise_prepare_benchctl "$root" || return $?
  local max_cpu=${PERFORMANCE_EXTERNAL_MAX_CPU_PERCENT:-50}
  if "$CONTINUOUS_NOISE_BENCHCTL" check-external-noise \
    --max-aggregate-cpu-percent "$max_cpu" --json-out "$evidence"; then rc=0; else rc=$?; fi
  if (( rc != 0 )); then
    echo "refusing performance case due to external host work ($phase)" >&2
  fi
  return "$rc"
}

performance_prepare_order() {
  local run_dir=$1 resume_policy=$2
  shift 2
  local -a jobs=("$@")
  local expected existing
  if [[ -s "$run_dir/jobs.txt" ]]; then
    expected=$(mktemp); existing=$(mktemp)
    printf '%s\n' "${jobs[@]}" | sort > "$expected"
    sort "$run_dir/jobs.txt" > "$existing"
    if ! cmp -s "$expected" "$existing"; then
      rm -f "$expected" "$existing"
      echo "refusing performance resume: jobs.txt changed" >&2
      return 2
    fi
    rm -f "$expected" "$existing"
    mapfile -t ORDERED < "$run_dir/jobs.txt"
  else
    mapfile -t ORDERED < <(printf '%s\n' "${jobs[@]}" | shuf)
    printf '%s\n' "${ORDERED[@]}" > "$run_dir/jobs.txt"
  fi
  if [[ "$resume_policy" == reshuffle-remaining ]]; then
    mapfile -t ORDERED < <(printf '%s\n' "${ORDERED[@]}" | shuf)
  fi
}

performance_run_with_continuous_noise() {
  continuous_noise_run "$@"
}

performance_continuous_rejected() {
  continuous_noise_rejected "$@"
}

performance_preserve_noise_rejection() {
  continuous_noise_preserve_rejection "$@"
}
