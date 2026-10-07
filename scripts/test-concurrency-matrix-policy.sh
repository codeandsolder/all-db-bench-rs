#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$ROOT/scripts/concurrency-matrix-policy.sh"

[[ $(kv_concurrency_ops smoke tiny-txn 3000 100 5000) == 300 ]]
[[ $(kv_concurrency_ops quick tiny-txn 50000 100 100000) == 1000 ]]
[[ $(kv_concurrency_ops full tiny-txn 250000 100 1000000) == 2500 ]]
[[ $(kv_concurrency_ops quick point-read 50000 100 100000) == 200000 ]]
[[ $(kv_concurrency_ops quick range-scan 50000 100 100000) == 1000 ]]
[[ $(kv_concurrency_ops smoke range-scan 3000 100 5000) == 30 ]]

[[ $(record_concurrency_ops quick record-concurrency-core point-read 24000) == 6000 ]]
[[ $(record_concurrency_ops quick record-concurrency-core indexed-read 24000) == 10000 ]]
[[ $(record_concurrency_ops quick record-concurrency-core read-heavy 24000) == 3500 ]]
[[ $(record_concurrency_ops quick record-concurrency-core tiny-txn 24000) == 2000 ]]
[[ $(record_concurrency_ops quick record-concurrency-core write-burst 24000) == 4000 ]]
[[ $(record_concurrency_ops quick record-concurrency-txn-1 write-burst 24000) == 1400 ]]
[[ $(record_concurrency_ops quick record-concurrency-txn-1000 write-burst 24000) == 8000 ]]
[[ $(record_concurrency_ops full record-concurrency-txn-1000 write-burst 80000) == 16000 ]]
[[ $(record_concurrency_ops smoke record-concurrency-txn-1 write-burst 2000) == 2000 ]]

echo concurrency-matrix-policy-ok
