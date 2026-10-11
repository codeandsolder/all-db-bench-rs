#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

runners=(
  run-kv-matrix.sh
  run-record-matrix.sh
  run-kv-concurrency-matrix.sh
  run-kv-concurrency-plan.sh
  run-record-concurrency-matrix.sh
  run-record-concurrency-plan.sh
  run-kv-sustained-matrix.sh
  run-record-sustained-matrix.sh
  run-reopen-matrix.sh
)
for runner in "${runners[@]}"; do
  path="$ROOT/scripts/$runner"
  for forbidden in \
    'check_io_quiet "after:$case_id"' \
    'performance_check_io_quiet "$PROFILE" "after:$case_id"' \
    'concurrency_check_io_quiet "$PROFILE" "after:$case_id"'
  do
    if rg -Fq "$forbidden" "$path"; then
      echo "post-case I/O PSI selector returned in $runner: $forbidden" >&2
      exit 1
    fi
  done
  if rg -q '(performance_scrub_case_pressure|concurrency_scrub_case_pressure)' "$path"; then
    echo "in-window pressure post-selector returned in $runner" >&2
    exit 1
  fi
  case "$runner" in
    run-record-matrix.sh)
      rg -Fq 'ADMISSION_POLICY=pre-io+pre/continuous/post-external-v3' "$path" && \
      rg -q 'continuous_noise_guard_sha256' "$path" && \
      rg -q 'run-with-continuous-noise.py' "$path" || {
        echo "missing continuous record-baseline admission provenance in $runner" >&2
        exit 1
      }
      ;;
    run-kv-matrix.sh|run-kv-sustained-matrix.sh|run-record-sustained-matrix.sh|run-reopen-matrix.sh)
      rg -q 'PERFORMANCE_ADMISSION_POLICY' "$path" && \
      rg -q 'continuous_noise_guard_sha256' "$path" && \
      rg -q 'noise_during=' "$path" && \
      rg -q 'performance_run_with_continuous_noise' "$path" || {
        echo "missing continuous performance admission provenance in $runner" >&2
        exit 1
      }
      ;;
    run-kv-concurrency-matrix.sh|run-kv-concurrency-plan.sh|run-record-concurrency-matrix.sh|run-record-concurrency-plan.sh)
      rg -q 'continuous_noise_guard_sha256' "$path" && \
      rg -q 'CONCURRENCY_ADMISSION_POLICY' "$path" && \
      rg -q 'noise_during=' "$path" && \
      rg -q 'concurrency_run_with_continuous_noise' "$path" || {
        echo "missing continuous concurrency admission provenance in $runner" >&2
        exit 1
      }
      ;;
    *)
      rg -q 'admission_policy.*pre-io\+pre/post-external-v2' "$path" || {
        echo "missing corrected admission provenance in $runner" >&2
        exit 1
      }
      ;;
  esac
done

for supervisor in run-record-quick-idle.py run-record-sizing-followups.py run-kv-sizing-followups.py; do
  if rg -q 'scrub_short_pressure' "$ROOT/scripts/$supervisor"; then
    echo "short-pressure acceptance selector returned in $supervisor" >&2
    exit 1
  fi
done

for common in performance-runner-common.sh concurrency-runner-common.sh; do
  rg -Fq '${CASE_MIN_FREE_GIB:-${PERFORMANCE_MIN_FREE_GIB:-${MIN_FREE_GIB:-8}}}' "$ROOT/scripts/$common" || {
    echo "missing profile-aware free-space admission floor in $common" >&2
    exit 1
  }
done

for baseline in run-record-matrix.sh; do
  rg -Fq 'CASE_MIN_FREE_GIB=${PERFORMANCE_MIN_FREE_GIB:-$MIN_FREE_GIB}' "$ROOT/scripts/$baseline" && \
  rg -Fq 'min_gib=${CASE_MIN_FREE_GIB}' "$ROOT/scripts/$baseline" || {
    echo "missing profile-aware free-space admission floor in $baseline" >&2
    exit 1
  }
done

for runner in \
  run-kv-matrix.sh run-record-matrix.sh \
  run-kv-concurrency-matrix.sh run-kv-concurrency-plan.sh \
  run-record-concurrency-matrix.sh run-record-concurrency-plan.sh \
  run-kv-sustained-matrix.sh run-record-sustained-matrix.sh run-reopen-matrix.sh
do
  rg -q '"initial_min_free_gib"' "$ROOT/scripts/$runner" && \
  rg -q '"case_min_free_gib"' "$ROOT/scripts/$runner" || {
    echo "missing free-space provenance in $runner" >&2
    exit 1
  }
done

echo performance-admission-ok

rg -q '^performance_preserve_noise_rejection()' "$ROOT/scripts/performance-runner-common.sh" || {
  echo "missing performance rejection archive wrapper" >&2
  exit 1
}
rg -q 'continuous_noise_preserve_rejection' "$ROOT/scripts/performance-runner-common.sh" || {
  echo "performance rejection archive wrapper does not delegate to continuous helper" >&2
  exit 1
}
