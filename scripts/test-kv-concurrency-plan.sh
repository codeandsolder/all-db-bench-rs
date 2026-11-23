#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bash -n "$ROOT/scripts/run-kv-concurrency-plan.sh"
PLAN=${1:?usage: test-kv-concurrency-plan.sh PLAN.json}
[[ -s "$PLAN" ]] || { echo "Missing plan fixture: $PLAN" >&2; exit 2; }
{
  tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
  "$ROOT/scripts/benchctl.sh" kv-concurrency-plan-materialize "$PLAN" "$tmp/plan.tsv" "$tmp/meta.json"
  [[ $(wc -l < "$tmp/plan.tsv") -eq 297 ]]
  jq -e '.plan_version == 1 and .expect_trials == 1 and .case_count == 297' "$tmp/meta.json" >/dev/null
}
echo kv-concurrency-plan-ok

# v1 plans must remain runnable by historical binaries that predate state-evolution flags.
rg -q 'PLAN_VERSION >= 2' "$ROOT/scripts/run-kv-concurrency-plan.sh"

# v2 plans prepare and cleanly close case-private DBs before the fresh timing gate.
rg -q -- '--prepare-only' "$ROOT/scripts/run-kv-concurrency-plan.sh"
rg -q -- '--reuse-db' "$ROOT/scripts/run-kv-concurrency-plan.sh"
rg -q 'case-private-clean-close-v1' "$ROOT/scripts/run-kv-concurrency-plan.sh"
rg -q 'runner_sha256' "$ROOT/scripts/run-kv-concurrency-plan.sh"
rg -q 'BENCH_BIN_PREVERIFIED' "$ROOT/scripts/run-kv-concurrency-plan.sh"
