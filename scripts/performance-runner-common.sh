#!/usr/bin/env bash

performance_check_io_quiet() {
  local profile=$1 phase=$2 threshold=${3:-5.0}
  [[ "$profile" == smoke || "${ALLOW_BUSY:-0}" == 1 ]] && return 0
  local pressure
  pressure=$(awk '/^full / {for(i=1;i<=NF;i++) if($i ~ /^avg10=/){split($i,a,"="); print a[2]}}' /proc/pressure/io)
  if ! awk -v p="${pressure:-0}" -v t="$threshold" 'BEGIN{exit !(p<=t)}'; then
    echo "refusing performance case under I/O pressure ($phase): ${pressure}%" >&2
    return 75
  fi
}

performance_check_external_noise() {
  local root=$1 profile=$2 phase=$3 evidence=$4 rc
  [[ "$profile" == smoke || "${ALLOW_EXTERNAL_NOISE:-0}" == 1 ]] && return 0
  if uv run --script "$root/scripts/check-external-noise.py" --json-out "$evidence"; then rc=0; else rc=$?; fi
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
