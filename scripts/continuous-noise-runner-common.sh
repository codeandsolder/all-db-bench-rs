#!/usr/bin/env bash

CONTINUOUS_ADMISSION_POLICY=${CONTINUOUS_ADMISSION_POLICY:-pre-io+pre/continuous/post-external-v3}
CONTINUOUS_NOISE_SAMPLE_MS=${CONTINUOUS_NOISE_SAMPLE_MS:-250}
CONTINUOUS_NOISE_MAX_CPU_PERCENT=${CONTINUOUS_NOISE_MAX_CPU_PERCENT:-50}
CONTINUOUS_NOISE_MAX_IO_AVERAGE_MIB_S=${CONTINUOUS_NOISE_MAX_IO_AVERAGE_MIB_S:-2}
CONTINUOUS_NOISE_MAX_IO_RATE_MIB_S=${CONTINUOUS_NOISE_MAX_IO_RATE_MIB_S:-8}

continuous_noise_run() {
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

continuous_noise_rejected() {
  local evidence=$1
  [[ -s "$evidence" ]] && jq -e '.contaminated == true' "$evidence" >/dev/null 2>&1
}

continuous_noise_preserve_rejection() {
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
