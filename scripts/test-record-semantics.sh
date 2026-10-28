#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
RECORD="$ROOT/src/record/engine.rs"
ROCKS="$ROOT/engines/surrealdb-rocksdb/src/lib.rs"
RUNNER="$ROOT/scripts/run-record-matrix.sh"
WIDE="$ROOT/scripts/run-record-wide-matrix.sh"
MAIN="$ROOT/tools/benchctl/src/main.rs"
SUPERVISOR="$ROOT/tools/benchctl/src/baseline_supervisor.rs"
GENERATION="$ROOT/tools/benchctl/src/baseline_generation.rs"

require() {
  local needle=$1 file=$2 message=$3
  grep -Fq -- "$needle" "$file" || { echo "$message" >&2; exit 1; }
}
reject() {
  local needle=$1 file=$2 message=$3
  if grep -Fq -- "$needle" "$file"; then echo "$message" >&2; exit 1; fi
}

# Record benchmark binding/materialization semantics.
reject 'id.to_string()' "$RECORD" 'Turso record id regressed to string binding'
reject 'group.to_string()' "$RECORD" 'Turso group id regressed to string binding'
reject 'data.bucket.to_string()' "$RECORD" 'Turso bucket regressed to string binding'
require 'stmt.query((id as i64,)).await?' "$RECORD" 'missing integer Turso record-id binding'
require 'stmt.query((group as i64,)).await?' "$RECORD" 'missing integer Turso group binding'
require 'data.payload.as_str()' "$RECORD" 'missing borrowed payload binding'
for source in "$RECORD" "$ROCKS"; do
  require 'SELECT bucket, payload FROM ONLY $id' "$source" 'Surreal point read no longer projects only compared fields'
  reject 'db.select(("item", id as i64))' "$source" 'Surreal point read regressed to full-record materialization'
done
require 'UPSERT $id CONTENT $data RETURN NONE' "$RECORD" 'Surreal write no longer suppresses returned record'
require 'RETURN NONE;\n' "$RECORD" 'Surreal batch write no longer suppresses returned records'
reject 'let _: Option<RecordData> =' "$RECORD" 'Surreal write materializes an unused returned record'
reject "payload: '{}'" "$RECORD" 'record payload regressed to formatted SQL literal'

# Record matrix acceptance must pin current read/write semantics and continuous admission.
for needle in \
  'READ_MATERIALIZATION=full-record-v1' \
  'WRITE_MATERIALIZATION=no-return-v1' \
  '--arg read_materialization "$READ_MATERIALIZATION"' \
  '--arg write_materialization "$WRITE_MATERIALIZATION"' \
  'read_materialization:$read_materialization' \
  'write_materialization:$write_materialization' \
  '.read_materialization == $read_expected' \
  '.write_materialization == $write_expected' \
  'record result semantic identity mismatch' \
  'run-with-continuous-noise' \
  'continuous_noise_guard_sha256' \
  'ADMISSION_POLICY=pre-io+pre/continuous/post-external-v3' \
  'if ((rc==75))' \
  'preserve_noise_rejection' \
  '"continuous"' \
  '"post-external"' \
  'noise/rejected' \
  'noise/rejections.ndjson'; do
  require "$needle" "$RUNNER" "missing record-matrix semantic/admission contract: $needle"
done
for needle in \
  'READ_MATERIALIZATION=full-record-v1' \
  'WRITE_MATERIALIZATION=no-return-v1' \
  '.read_materialization == $read_expected' \
  '.write_materialization == $write_expected' \
  'record result semantic identity mismatch'; do
  require "$needle" "$WIDE" "missing wide-record semantic contract: $needle"
done

# Native baseline control plane must require explicit pinned record binaries and generation inputs.
require 'BaselineQuickIdle' "$MAIN" 'missing native baseline quick supervisor command'
require 'BaselineSizingFollowups' "$MAIN" 'missing native baseline followup command'
require 'rocks_bench_bin: Option<PathBuf>' "$MAIN" 'record supervisor lost explicit Rocks binary argument'
require 'expected_rocks_bench_sha256: Option<String>' "$MAIN" 'record supervisor lost pinned Rocks SHA argument'
reject 'recordbench-ae74103b847171d1' "$SUPERVISOR" 'historical record binary hardcoded into native supervisor'
reject 'surrealdb-rocksdb-recordbench-c3978a3b66a24edd' "$SUPERVISOR" 'historical Rocks binary hardcoded into native supervisor'
reject '20261009-record-full-v1-resize-v1' "$SUPERVISOR" 'historical record resize prefix hardcoded into native supervisor'
require '"baseline-sizing-followups".into()' "$GENERATION" 'generation no longer schedules native sizing followups'
require 'paths.record_resize_prefix.clone()' "$GENERATION" 'record followups no longer use generation-scoped prefix'
require 'FinalizeBaselineSizing' "$MAIN" 'missing native baseline finalizer'
require 'record_stock_results: Option<PathBuf>' "$MAIN" 'finalizer lost explicit record stock input'
require 'record_audit: Option<PathBuf>' "$MAIN" 'finalizer lost explicit record audit input'
require '--record-stock-results is required for the selected lane' "$MAIN" 'finalizer no longer fails closed without record stock input'
require '--record-audit is required for the selected lane' "$MAIN" 'finalizer no longer fails closed without record audit input'

echo record-semantics-ok
