#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
TMP=$(mktemp -d /tmp/db-bench-support-test.XXXXXX)
R0="tmp-support-kv-$$"
R1="tmp-support-record-$$"
R2="tmp-support-kvc-$$"
R3="tmp-support-recordc-$$"
R4="tmp-support-record-plan-$$"
R5="tmp-support-sustained-$$"
R6="tmp-support-reopen-$$"
trap 'rm -rf -- "$TMP" "results/runs/$R0" "results/runs/$R1" "results/runs/$R2" "results/runs/$R3" "results/runs/$R4" "results/runs/$R5" "results/runs/$R6"' EXIT
SHA=$(sha256sum /bin/true | awk '{print $1}')
# Publication provenance must hash the authoritative toolchain manifest.
rg -Fq 'Cargo.lock Cargo.toml rust-toolchain.toml ' "$ROOT/scripts/capture-host-metadata.sh"
[[ -s "$ROOT/rust-toolchain.toml" ]]

MATRIX_PLAN_ONLY=1 DBBENCH_STORAGE_ROOT="$TMP/kv" BENCH_BIN=/bin/true \
  ENGINES_OVERRIDE=redb WORKLOADS_OVERRIDE=point-read DURABILITIES_OVERRIDE=sync RUN_ID="$R0" \
  scripts/run-kv-matrix.sh smoke >/dev/null
jq -e '.lane == "kv" and .initial_min_free_gib == 2 and .case_min_free_gib == "2"' \
  "results/runs/$R0/support.json" >/dev/null

sed -i 's/"continuous_noise_max_cpu_percent":50/"continuous_noise_max_cpu_percent":50.0/' "results/runs/$R0/support.json"
CONTINUOUS_NOISE_MAX_CPU_PERCENT=50.0 MATRIX_PLAN_ONLY=1 DBBENCH_STORAGE_ROOT="$TMP/kv" BENCH_BIN=/bin/true \
  ENGINES_OVERRIDE=redb WORKLOADS_OVERRIDE=point-read DURABILITIES_OVERRIDE=sync RUN_ID="$R0" \
  scripts/run-kv-matrix.sh smoke >/dev/null
rg -q '"continuous_noise_max_cpu_percent":50.0' "results/runs/$R0/support.json"

MATRIX_PLAN_ONLY=1 DBBENCH_STORAGE_ROOT="$TMP/record" BENCH_BIN=/bin/true ROCKS_BENCH_BIN=/bin/true \
  ENGINES_OVERRIDE=surrealdb WORKLOADS_OVERRIDE=point-read DURABILITIES_OVERRIDE=sync RUN_ID="$R1" \
  scripts/run-record-matrix.sh smoke >/dev/null
jq -e '.lane == "record" and .read_materialization == "full-record-v1" and .write_materialization == "no-return-v1"' \
  "results/runs/$R1/support.json" >/dev/null

MATRIX_PLAN_ONLY=1 DBBENCH_STORAGE_ROOT="$TMP/kvc" BENCH_BIN=/bin/true BENCH_BIN_SHA256="$SHA" \
  ENGINES_OVERRIDE=redb RUN_ID="$R2" scripts/run-kv-concurrency-matrix.sh smoke >/dev/null
jq -e '.lane == "kv-concurrency" and .shared_database == true and .import_manifest_sha256 == null' \
  "results/runs/$R2/support.json" >/dev/null

MATRIX_PLAN_ONLY=1 DBBENCH_STORAGE_ROOT="$TMP/recordc" BENCH_BIN=/bin/true BENCH_BIN_SHA256="$SHA" \
  ROCKS_BENCH_BIN=/bin/true ROCKS_BENCH_BIN_SHA256="$SHA" ENGINES_OVERRIDE=surrealdb RUN_ID="$R3" \
  scripts/run-record-concurrency-matrix.sh smoke >/dev/null
jq -e '.lane == "record-concurrency" and .payload_bytes == 128 and .case_count > 0' \
  "results/runs/$R3/support.json" >/dev/null

scripts/benchctl.sh record-concurrency-plan-calibration --repo "$ROOT" --out "$TMP/record-plan.json" >/dev/null
COMMIT=$(git rev-parse HEAD)
MATRIX_PLAN_ONLY=1 DBBENCH_STORAGE_ROOT="$TMP/record-plan-data" BENCH_BIN=/bin/true BENCH_BIN_SHA256="$SHA" BENCH_BIN_PREVERIFIED=1 \
  ROCKS_BENCH_BIN=/bin/true ROCKS_BENCH_BIN_SHA256="$SHA" BENCH_SOURCE_COMMIT="$COMMIT" ROCKS_BENCH_SOURCE_COMMIT="$COMMIT" \
  scripts/run-record-concurrency-plan.sh "$TMP/record-plan.json" "$R4" >/dev/null
jq -e '.lane == "record-concurrency" and .read_materialization == "full-record-v1" and .write_materialization == "no-return-v1" and .case_count > 0' \
  "results/runs/$R4/support.json" >/dev/null

# The measured KV lanes retain exact support identity in plan-only mode.
MATRIX_PLAN_ONLY=1 DBBENCH_STORAGE_ROOT="$TMP/sustained" BENCH_BIN=/bin/true \
  BENCH_SOURCE_COMMIT="$COMMIT" RUN_ID="$R5" scripts/run-kv-sustained-matrix.sh smoke >/dev/null
jq -e --arg commit "$COMMIT" '.lane == "kv-sustained" and .benchmark_source_commit == $commit and .build_profile == "external" and .case_count > 0' \
  "results/runs/$R5/support.json" >/dev/null

MATRIX_PLAN_ONLY=1 DBBENCH_STORAGE_ROOT="$TMP/reopen" BENCH_BIN=/bin/true \
  BENCH_SOURCE_COMMIT="$COMMIT" RUN_ID="$R6" scripts/run-reopen-matrix.sh smoke warm >/dev/null
jq -e --arg commit "$COMMIT" '.lane == "kv-reopen" and .benchmark_source_commit == $commit and .build_profile == "external" and .case_count == 50' \
  "results/runs/$R6/support.json" >/dev/null

echo support-serializers-ok
