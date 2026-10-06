#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=kv-matrix-policy.sh
source "$ROOT/scripts/kv-matrix-policy.sh"

eq() { [[ "$1" == "$2" ]] || { echo "expected $2, got $1" >&2; exit 1; }; }
eq "$(kv_effective_ops redb range-scan 50000 100000)" 50000
eq "$(kv_effective_ops lsmdb point-read 50000 100000)" 50000
eq "$(kv_effective_ops lsmdb range-scan 100 100)" 100
eq "$(kv_effective_ops lsmdb range-scan 50000 100000)" 1000
eq "$(kv_effective_ops lsmdb range-scan 250000 1000000)" 100
eq "$(kv_effective_ops lsmdb range-scan 250000 10000000)" 50
if kv_effective_ops lsmdb range-scan 0 100000 >/dev/null 2>&1; then
  echo 'zero profile ops unexpectedly accepted' >&2; exit 1
fi
if kv_effective_ops lsmdb range-scan 50000 nope >/dev/null 2>&1; then
  echo 'invalid record count unexpectedly accepted' >&2; exit 1
fi
# Import provenance is runner-level, but the policy test also pins the expected
# scaled counts that imported legacy cases must never impersonate.
eq "$(kv_effective_ops lsmdb range-scan 50000 100000 50000000 50)" 1000
