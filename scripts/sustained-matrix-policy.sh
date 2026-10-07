#!/usr/bin/env bash
# Smoke-compatible and performance-grade sustained-workload budgets.
# Fields: records ops window_ops value_or_payload_bytes.

kv_sustained_budget() {
  local profile=$1 scenario=$2 txn=${3:-100}
  case "$profile:$scenario:$txn" in
    smoke:core:*) echo "5000 10000 1000 256" ;;
    smoke:stress:*) echo "5000 10000 1000 4096" ;;
    smoke:relaxed:*) echo "5000 10000 1000 256" ;;
    smoke:txn:*) echo "5000 5000 1000 256" ;;
    quick:core:*) echo "100000 25000 2500 256" ;;
    quick:stress:*) echo "50000 5000 625 4096" ;;
    quick:relaxed:*) echo "100000 20000 2000 256" ;;
    quick:txn:1) echo "50000 1500 150 256" ;;
    quick:txn:1000) echo "50000 10000 1000 256" ;;
    full:core:*) echo "1000000 2000000 50000 256" ;;
    full:stress:*) echo "250000 1000000 25000 4096" ;;
    full:relaxed:*) echo "500000 1000000 25000 256" ;;
    full:txn:*) echo "100000 100000 5000 256" ;;
    *) return 2 ;;
  esac
}

record_sustained_budget() {
  local profile=$1 scenario=$2 txn=${3:-100}
  case "$profile:$scenario:$txn" in
    smoke:core:*) echo "2000 4000 1000 256" ;;
    smoke:stress:*) echo "1000 4000 1000 4096" ;;
    smoke:relaxed:*) echo "2000 4000 1000 256" ;;
    smoke:txn:*) echo "1000 4000 1000 256" ;;
    quick:core:*) echo "50000 10000 1000 256" ;;
    quick:stress:*) echo "20000 8000 1000 4096" ;;
    quick:relaxed:*) echo "50000 8000 1000 256" ;;
    quick:txn:1) echo "20000 1200 120 256" ;;
    quick:txn:1000) echo "20000 8000 1000 256" ;;
    full:core:*) echo "500000 1000000 25000 256" ;;
    full:stress:*) echo "100000 500000 25000 4096" ;;
    full:relaxed:*) echo "250000 500000 25000 256" ;;
    full:txn:*) echo "50000 100000 5000 256" ;;
    *) return 2 ;;
  esac
}

kv_sustained_expected_cases() {
  case "$1" in smoke) echo 86 ;; quick) echo 354 ;; full) echo 590 ;; *) return 2 ;; esac
}
record_sustained_expected_cases() {
  case "$1" in smoke) echo 24 ;; quick) echo 90 ;; full) echo 150 ;; *) return 2 ;; esac
}
