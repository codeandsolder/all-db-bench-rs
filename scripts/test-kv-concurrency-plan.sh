#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bash -n "$ROOT/scripts/run-kv-concurrency-plan.sh"
PLAN=/srv/scratch/db-bench-work/concurrency-sizing-audit/20261007-kv-concurrency-missing-client-groups-v1.json
if [[ -s "$PLAN" ]]; then
  tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
  uv run --script "$ROOT/scripts/validate-concurrency-plan.py" "$PLAN" "$tmp/plan.tsv" "$tmp/meta.json"
  [[ $(wc -l < "$tmp/plan.tsv") -eq 297 ]]
  jq -e '.plan_version == 1 and .expect_trials == 1 and .case_count == 297' "$tmp/meta.json" >/dev/null
fi
echo kv-concurrency-plan-ok

# v1 plans must remain runnable by historical binaries that predate state-evolution flags.
rg -q 'PLAN_VERSION >= 2' "$ROOT/scripts/run-kv-concurrency-plan.sh"

# v2 plans prepare and cleanly close case-private DBs before the fresh timing gate.
rg -q -- '--prepare-only' "$ROOT/scripts/run-kv-concurrency-plan.sh"
rg -q -- '--reuse-db' "$ROOT/scripts/run-kv-concurrency-plan.sh"
rg -q 'case-private-clean-close-v1' "$ROOT/scripts/run-kv-concurrency-plan.sh"
rg -q 'runner_sha256' "$ROOT/scripts/run-kv-concurrency-plan.sh"
rg -q 'BENCH_BIN_PREVERIFIED' "$ROOT/scripts/run-kv-concurrency-plan.sh"
