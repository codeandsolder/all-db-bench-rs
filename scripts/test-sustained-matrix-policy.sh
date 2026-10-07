#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$ROOT/scripts/sustained-matrix-policy.sh"
[[ $(kv_sustained_expected_cases smoke) == 86 ]]
[[ $(kv_sustained_expected_cases quick) == 354 ]]
[[ $(kv_sustained_expected_cases full) == 590 ]]
[[ $(record_sustained_expected_cases smoke) == 32 ]]
[[ $(record_sustained_expected_cases quick) == 120 ]]
[[ $(record_sustained_expected_cases full) == 200 ]]
[[ $(kv_sustained_budget quick core 100) == "100000 25000 2500 256" ]]
[[ $(kv_sustained_budget quick stress 100) == "50000 5000 625 4096" ]]
[[ $(kv_sustained_budget quick relaxed 100) == "100000 20000 2000 256" ]]
[[ $(kv_sustained_budget quick txn 1) == "50000 1500 150 256" ]]
[[ $(kv_sustained_budget quick txn 1000) == "50000 10000 1000 256" ]]
[[ $(record_sustained_budget quick core 100) == "50000 10000 1000 256" ]]
[[ $(record_sustained_budget quick stress 100) == "20000 8000 1000 4096" ]]
[[ $(record_sustained_budget quick relaxed 100) == "50000 8000 1000 256" ]]
[[ $(record_sustained_budget quick txn 1) == "20000 1200 120 256" ]]
[[ $(record_sustained_budget quick txn 1000) == "20000 8000 1000 256" ]]
# Smoke semantics remain byte-for-byte equivalent to the accepted smoke profile.
[[ $(kv_sustained_budget smoke core 100) == "5000 10000 1000 256" ]]
[[ $(kv_sustained_budget smoke stress 100) == "5000 10000 1000 4096" ]]
[[ $(kv_sustained_budget smoke relaxed 100) == "5000 10000 1000 256" ]]
[[ $(kv_sustained_budget smoke txn 1) == "5000 5000 1000 256" ]]
[[ $(kv_sustained_budget smoke txn 1000) == "5000 5000 1000 256" ]]
[[ $(record_sustained_budget smoke core 100) == "2000 4000 1000 256" ]]
[[ $(record_sustained_budget smoke stress 100) == "1000 4000 1000 4096" ]]
[[ $(record_sustained_budget smoke relaxed 100) == "2000 4000 1000 256" ]]
[[ $(record_sustained_budget smoke txn 1) == "1000 4000 1000 256" ]]
[[ $(record_sustained_budget smoke txn 1000) == "1000 4000 1000 256" ]]
echo sustained-matrix-policy-ok
