#!/usr/bin/env bash

CONCURRENCY_ADMISSION_POLICY=${CONCURRENCY_ADMISSION_POLICY:-pre-io+pre/continuous/post-external-v3}
CONTINUOUS_NOISE_SAMPLE_MS=${CONTINUOUS_NOISE_SAMPLE_MS:-250}
CONTINUOUS_NOISE_MAX_CPU_PERCENT=${CONTINUOUS_NOISE_MAX_CPU_PERCENT:-50}
CONTINUOUS_NOISE_MAX_IO_AVERAGE_MIB_S=${CONTINUOUS_NOISE_MAX_IO_AVERAGE_MIB_S:-2}
CONTINUOUS_NOISE_MAX_IO_RATE_MIB_S=${CONTINUOUS_NOISE_MAX_IO_RATE_MIB_S:-8}

concurrency_check_free_space() {
  local profile=$1 phase=$2 path=${3:-${PERFORMANCE_FREE_SPACE_PATH:-${DATA_DIR:-$PWD}}}
  [[ "$profile" == smoke ]] && return 0
  local min_gib=${CASE_MIN_FREE_GIB:-${PERFORMANCE_MIN_FREE_GIB:-${MIN_FREE_GIB:-8}}} free_kib min_kib
  free_kib=$(df -Pk -- "$path" | awk 'NR==2 {print $4}') || return 2
  min_kib=$(awk -v g="$min_gib" 'BEGIN { if (g < 0) exit 2; printf "%.0f", g * 1024 * 1024 }') || return 2
  if (( free_kib < min_kib )); then
    echo "refusing concurrency performance case under low free space ($phase): free_kib=$free_kib required_kib=$min_kib path=$path" >&2
    return 75
  fi
}

concurrency_check_io_quiet() {
  local profile=$1 phase=$2 threshold=${3:-5.0}
  [[ "$profile" == smoke ]] && return 0
  concurrency_check_free_space "$profile" "$phase" || return $?
  [[ "${ALLOW_BUSY:-0}" == 1 ]] && return 0
  local pressure
  pressure=$(awk '/^full / {for(i=1;i<=NF;i++) if($i ~ /^avg10=/){split($i,a,"="); print a[2]}}' /proc/pressure/io)
  if ! awk -v p="${pressure:-0}" -v t="$threshold" 'BEGIN{exit !(p<=t)}'; then
    echo "refusing concurrency performance case under I/O pressure ($phase): ${pressure}%" >&2
    return 75
  fi
}

concurrency_check_external_noise() {
  local root=$1 profile=$2 phase=$3 evidence=$4 rc
  [[ "$profile" == smoke || "${ALLOW_EXTERNAL_NOISE:-0}" == 1 ]] && return 0
  if uv run --script "$root/scripts/check-external-noise.py" --json-out "$evidence"; then rc=0; else rc=$?; fi
  if (( rc != 0 )); then
    echo "refusing concurrency performance case due to external host work ($phase)" >&2
  fi
  return "$rc"
}

concurrency_prepare_order() {
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
      echo "refusing concurrency resume: jobs.txt changed" >&2
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

concurrency_run_with_continuous_noise() {
  local root=$1 evidence=$2
  shift 2
  uv run --script "$root/scripts/run-with-continuous-noise.py" \
    --json-out "$evidence" \
    --sample-ms "$CONTINUOUS_NOISE_SAMPLE_MS" \
    --max-foreign-cpu-percent "$CONTINUOUS_NOISE_MAX_CPU_PERCENT" \
    --max-foreign-io-average-mib-s "$CONTINUOUS_NOISE_MAX_IO_AVERAGE_MIB_S" \
    --max-foreign-io-rate-mib-s "$CONTINUOUS_NOISE_MAX_IO_RATE_MIB_S" \
    -- "$@"
}

concurrency_continuous_rejected() {
  local evidence=$1
  [[ -s "$evidence" ]] && jq -e '.contaminated == true' "$evidence" >/dev/null 2>&1
}

concurrency_preserve_noise_rejection() {
  local run_dir=$1 case_id=$2 reason=$3 before=$4 during=$5 after=$6 stamp base
  stamp=$(date -u +%Y%m%dT%H%M%S.%N)
  mkdir -p "$run_dir/noise/rejected"
  base="$run_dir/noise/rejected/$case_id.$stamp"
  [[ -s "$before" ]] && cp -- "$before" "$base.before.json"
  [[ -s "$during" ]] && cp -- "$during" "$base.during.json"
  [[ -s "$after" ]] && cp -- "$after" "$base.after.json"
  jq -cn --arg case_id "$case_id" --arg reason "$reason" --arg attempt_prefix "$base" \
    '{case_id:$case_id,reason:$reason,attempt_prefix:$attempt_prefix}' >>"$run_dir/noise/rejections.ndjson"
}
