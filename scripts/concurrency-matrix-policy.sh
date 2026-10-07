#!/usr/bin/env bash
# Shared operation budgets for concurrency performance lanes.
# Total logical work remains fixed across client counts within each case family.

concurrency_case_timeout_s() {
  case "$1" in
    smoke) echo 60 ;;
    quick) echo 180 ;;
    full) echo 1800 ;;
    *) return 2 ;;
  esac
}

kv_concurrency_ops() {
  local profile=$1 workload=$2 default_ops=$3 scan_len=$4 records=$5
  case "$workload" in
    tiny-txn)
      case "$profile" in
        smoke) echo 300 ;;
        quick) echo 1000 ;;
        full) echo 2500 ;;
        *) return 2 ;;
      esac
      ;;
    point-read)
      if [[ "$profile" == quick ]]; then echo 200000; else echo "$default_ops"; fi
      ;;
    range-scan)
      local ops=$((default_ops / scan_len))
      if [[ "$profile" == quick ]]; then ops=1000; fi
      (( ops < 1 )) && ops=1
      echo "$ops"
      ;;
    *) echo "$default_ops" ;;
  esac
}

record_concurrency_ops() {
  local profile=$1 scenario=$2 workload=$3 default_ops=$4
  case "$profile" in
    smoke)
      echo "$default_ops"
      ;;
    quick)
      case "$scenario:$workload" in
        record-concurrency-core:point-read) echo 6000 ;;
        record-concurrency-core:indexed-read) echo 10000 ;;
        record-concurrency-core:read-heavy) echo 3500 ;;
        record-concurrency-core:tiny-txn) echo 2000 ;;
        record-concurrency-core:write-burst) echo 4000 ;;
        record-concurrency-relaxed:read-heavy|record-concurrency-relaxed:write-burst) echo 6000 ;;
        record-concurrency-large-payload:read-heavy|record-concurrency-large-payload:write-burst) echo 2500 ;;
        record-concurrency-hotset:read-heavy) echo 3000 ;;
        record-concurrency-txn-1:write-burst) echo 1400 ;;
        record-concurrency-txn-1000:write-burst) echo 8000 ;;
        *) echo "$default_ops" ;;
      esac
      ;;
    full)
      case "$scenario:$workload" in
        record-concurrency-core:point-read) echo 12000 ;;
        record-concurrency-core:indexed-read) echo 20000 ;;
        record-concurrency-core:read-heavy) echo 7000 ;;
        record-concurrency-core:tiny-txn) echo 4000 ;;
        record-concurrency-core:write-burst) echo 8000 ;;
        record-concurrency-relaxed:read-heavy|record-concurrency-relaxed:write-burst) echo 12000 ;;
        record-concurrency-large-payload:read-heavy|record-concurrency-large-payload:write-burst) echo 5000 ;;
        record-concurrency-hotset:read-heavy) echo 6000 ;;
        record-concurrency-txn-1:write-burst) echo 2800 ;;
        record-concurrency-txn-1000:write-burst) echo 16000 ;;
        *) echo "$default_ops" ;;
      esac
      ;;
    *) return 2 ;;
  esac
}
