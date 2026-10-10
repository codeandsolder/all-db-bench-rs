#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$ROOT/scripts/performance-runner-common.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/order"

performance_prepare_order "$tmp/order" fixed-initial "a" "b" "c"
[[ $(wc -l < "$tmp/order/jobs.txt") -eq 3 ]]
performance_prepare_order "$tmp/order" reshuffle-remaining "a" "b" "c"
set +e
performance_prepare_order "$tmp/order" fixed-initial "a" "b" "d"
order_rc=$?
set -e
[[ $order_rc -eq 2 ]]
! declare -F performance_scrub_case_pressure >/dev/null

DATA_DIR="$tmp"
set +e
PERFORMANCE_MIN_FREE_GIB=1000000 performance_check_io_quiet quick low-space >/dev/null 2>&1
low_space_rc=$?
ALLOW_BUSY=1 PERFORMANCE_MIN_FREE_GIB=1000000 performance_check_io_quiet quick allow-busy-low-space >/dev/null 2>&1
allow_busy_low_space_rc=$?
set -e
[[ $low_space_rc -eq 75 ]]
[[ $allow_busy_low_space_rc -eq 75 ]]
PERFORMANCE_MIN_FREE_GIB=1000000 performance_check_io_quiet smoke smoke-bypass
echo performance-runner-common-ok
