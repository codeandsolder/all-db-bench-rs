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
      rg -q 'run-with-continuous-noise' "$path" || {
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

if rg -q 'scrub_short_pressure' "$ROOT/tools/benchctl/src/baseline_supervisor.rs"; then
  echo "short-pressure acceptance selector returned in native baseline supervisor" >&2
  exit 1
fi

for common in performance-runner-common.sh concurrency-runner-common.sh; do
  rg -Fq '${CASE_MIN_FREE_GIB:-${PERFORMANCE_MIN_FREE_GIB:-${MIN_FREE_GIB:-8}}}' "$ROOT/scripts/$common" || {
    echo "missing profile-aware free-space admission floor in $common" >&2
    exit 1
  }
done

# The record lane delegates to the shared free-space and device gate.
rg -Fq 'CASE_MIN_FREE_GIB=${PERFORMANCE_MIN_FREE_GIB:-$MIN_FREE_GIB}' "$ROOT/scripts/run-record-matrix.sh"
rg -Fq 'performance_check_io_quiet "$PROFILE" "$1"' "$ROOT/scripts/run-record-matrix.sh"
rg -Fq 'local min_gib=${CASE_MIN_FREE_GIB' "$ROOT/scripts/performance-runner-common.sh"

for runner in \
  run-kv-matrix.sh run-record-matrix.sh \
  run-kv-concurrency-matrix.sh run-kv-concurrency-plan.sh \
  run-record-concurrency-matrix.sh run-record-concurrency-plan.sh \
  run-kv-sustained-matrix.sh run-record-sustained-matrix.sh run-reopen-matrix.sh
do
  rg -q 'initial_min_free_gib' "$ROOT/scripts/$runner" && \
  rg -q 'case_min_free_gib' "$ROOT/scripts/$runner" || {
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

rg -Fq 'MATRIX_PLAN_ONLY' "$ROOT/scripts/run-kv-matrix.sh" || {
  echo "raw-KV runner lost plan-only support" >&2
  exit 1
}

for runner in \
  run-kv-matrix.sh run-kv-concurrency-matrix.sh run-kv-concurrency-plan.sh \
  run-record-concurrency-matrix.sh run-record-concurrency-plan.sh \
  run-kv-sustained-matrix.sh run-record-sustained-matrix.sh run-reopen-matrix.sh
do
  rg -q 'continuous_noise_common_sha256' "$ROOT/scripts/$runner" || {
    echo "continuous helper provenance missing in $runner" >&2
    exit 1
  }
done
for runner in run-kv-concurrency-matrix.sh run-kv-concurrency-plan.sh run-record-concurrency-matrix.sh run-record-concurrency-plan.sh; do
  rg -q 'concurrency_runner_common_sha256' "$ROOT/scripts/$runner" || {
    echo "concurrency common provenance missing in $runner" >&2
    exit 1
  }
done

for runner in run-kv-concurrency-plan.sh run-record-concurrency-plan.sh; do
  rg -Fq 'MATRIX_PLAN_ONLY' "$ROOT/scripts/$runner" || {
    echo "concurrency plan runner lost plan-only support: $runner" >&2
    exit 1
  }
done

for runner in run-kv-concurrency-plan.sh run-record-concurrency-plan.sh; do
  rg -Fq 'benchctl.sh" support-identity' "$ROOT/scripts/$runner" && \
  rg -Fq -- '--expected-lane' "$ROOT/scripts/$runner" && \
  rg -Fq -- '--runner-name' "$ROOT/scripts/$runner" && \
  rg -Fq -- '--plan "$PLAN"' "$ROOT/scripts/$runner" || {
    echo "concurrency plan runner lost native support-identity gate: $runner" >&2
    exit 1
  }
done

# Both benchmark families enforce the same free-space admission contract.
# Test the profile floor separately from ALLOW_BUSY and bounded-mode I/O.
source "$ROOT/scripts/performance-runner-common.sh"
source "$ROOT/scripts/concurrency-runner-common.sh"
assert_runner_free_space_gate() (
  local checker=$1 rc
  export DATA_DIR="$ROOT"
  unset CASE_MIN_FREE_GIB
  if PERFORMANCE_MIN_FREE_GIB=1000000 "$checker" quick low-space >/dev/null 2>&1; then rc=0; else rc=$?; fi
  [[ $rc == 75 ]] || { echo "$checker: low free space was not rejected" >&2; exit 1; }
  if ALLOW_BUSY=1 PERFORMANCE_MIN_FREE_GIB=1000000 "$checker" quick busy-low-space >/dev/null 2>&1; then rc=0; else rc=$?; fi
  [[ $rc == 75 ]] || { echo "$checker: ALLOW_BUSY bypassed free-space gating" >&2; exit 1; }

  export CASE_MIN_FREE_GIB=1000000
  if "$checker" quick profile-floor >/dev/null 2>&1; then rc=0; else rc=$?; fi
  [[ $rc == 75 ]] || { echo "$checker: profile floor was not enforced" >&2; exit 1; }
  if ALLOW_BUSY=1 PERFORMANCE_MIN_FREE_GIB=0 CASE_MIN_FREE_GIB=0 "$checker" quick explicit-override >/dev/null 2>&1; then rc=0; else rc=$?; fi
  [[ $rc == 0 ]] || { echo "$checker: explicit zero floor was rejected: $rc" >&2; exit 1; }

  unset CASE_MIN_FREE_GIB
  PERFORMANCE_MIN_FREE_GIB=1000000 "$checker" smoke smoke-bypass
)
assert_runner_free_space_gate performance_check_io_quiet
assert_runner_free_space_gate concurrency_check_io_quiet
echo performance-free-space-admission-ok

# Every selected physical device must pass admission in native/bounded mode.
performance_check_free_space() { return 0; }
expect_device_status() {
  local expected=$1 actual
  shift
  if "$@"; then actual=0; else actual=$?; fi
  if ((actual != expected)); then
    printf 'expected admission status %s, got %s\n' "$expected" "$actual" >&2
    return 1
  fi
}
missing_device() (
  export DBBENCH_STORAGE_PROFILE_MODE=$1
  export PERFORMANCE_IO_QUIET_DEVICES_JSON='["/dev/dbbench-missing-device"]'
  performance_check_io_quiet quick regression >/dev/null 2>&1
)
expect_device_status 2 missing_device bounded
expect_device_status 2 missing_device native
expect_device_status 2 bash -c 'source "$1"; performance_check_free_space() { return 0; }; PERFORMANCE_IO_QUIET_DEVICES_JSON=broken performance_check_io_quiet quick malformed' _ "$ROOT/scripts/performance-runner-common.sh"
# Without a device, bounded mode falls back to PSI, not unconditional approval.
expect_device_status 75 bash -c 'source "$1"; performance_check_free_space() { return 0; }; PERFORMANCE_IO_QUIET_DEVICES_JSON="[]" DBBENCH_STORAGE_PROFILE_MODE=bounded performance_check_io_quiet quick fallback -1' _ "$ROOT/scripts/performance-runner-common.sh"
rg -Fq 'source "$ROOT/scripts/performance-runner-common.sh"' "$ROOT/scripts/run-record-matrix.sh"
rg -Fq 'source "$ROOT/scripts/performance-runner-common.sh"' "$ROOT/scripts/run-tsdb-matrix.sh"
rg -Fq 'source "$COMMON_DIR/performance-runner-common.sh"' "$ROOT/scripts/concurrency-runner-common.sh"
for runner in run-kv-concurrency-matrix.sh run-kv-concurrency-plan.sh run-record-concurrency-matrix.sh run-record-concurrency-plan.sh run-record-matrix.sh; do
  rg -q 'performance_common_sha256' "$ROOT/scripts/$runner" || { echo "missing shared admission helper identity in $runner" >&2; exit 1; }
done
rg -Fq '"io_admission_common": "$PERFORMANCE_COMMON_SHA"' "$ROOT/scripts/run-tsdb-matrix.sh"
echo performance-device-admission-ok
