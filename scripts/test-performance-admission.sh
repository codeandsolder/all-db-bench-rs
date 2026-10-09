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
  rg -q 'admission_policy.*pre-io\+pre/post-external-v2' "$path" || {
    echo "missing corrected admission provenance in $runner" >&2
    exit 1
  }
done

for supervisor in run-record-quick-idle.py run-record-sizing-followups.py run-kv-sizing-followups.py; do
  if rg -q 'scrub_short_pressure' "$ROOT/scripts/$supervisor"; then
    echo "short-pressure acceptance selector returned in $supervisor" >&2
    exit 1
  fi
done

echo performance-admission-ok
