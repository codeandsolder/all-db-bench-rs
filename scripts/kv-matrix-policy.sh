#!/usr/bin/env bash

# Return the operation count for one KV matrix case.
# Most cases use the profile operation count unchanged. lsm-db 1.0.0 bounded
# scans cannot seek to the lower bound: each run cursor starts at block zero,
# so a uniformly positioned range traverses roughly records/2 entries before
# returning the requested rows. Bound that hidden traversal work so the matrix
# measures the pathology without spending hours repeating the same O(N) cost.
kv_effective_ops() {
  local engine=$1 workload=$2 profile_ops=$3 records=$4
  local target_entries=${5:-50000000}
  local min_ops=${6:-50}

  [[ "$profile_ops" =~ ^[1-9][0-9]*$ ]] || return 2
  [[ "$records" =~ ^[1-9][0-9]*$ ]] || return 2
  [[ "$target_entries" =~ ^[1-9][0-9]*$ ]] || return 2
  [[ "$min_ops" =~ ^[1-9][0-9]*$ ]] || return 2

  if [[ "$engine" != lsmdb || "$workload" != range-scan ]]; then
    printf '%s\n' "$profile_ops"
    return 0
  fi

  # Expected pre-range traversal is approximately records/2 per random scan.
  # 2*target/records therefore targets target_entries of hidden traversal.
  local ops=$((2 * target_entries / records))
  (( ops < min_ops )) && ops=$min_ops
  (( ops > profile_ops )) && ops=$profile_ops
  printf '%s\n' "$ops"
}
