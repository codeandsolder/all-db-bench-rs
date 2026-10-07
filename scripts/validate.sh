#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

RUSTC_REAL=$(rustup which rustc --toolchain 1.99.0)
"$RUSTC_REAL" --version | rg '^rustc 1\.99\.0 '
"$ROOT/scripts/cargo-local-1.99.sh" --version
rg --version | head -1
uv --version
jq --version
fio --version
"$ROOT/scripts/ensure-sqlite-3.53.4.sh"

"$ROOT/scripts/cargo-local-1.99.sh" metadata --locked --no-deps --format-version 1 >/dev/null
"$ROOT/scripts/cargo-local-1.99.sh" fmt --all --check
rustfmt +1.99.0 --edition 2024 \
  engines/surrealdb-rocksdb/src/main.rs \
  engines/surrealdb-rocksdb/src/metrics.rs \
  engines/surrealdb-rocksdb/src/bin/surrealdb-rocksdb-recordconcurrency.rs \
  engines/surrealdb-rocksdb/src/bin/surrealdb-rocksdb-recordsustained.rs --check
bash -n scripts/*.sh
uv run python -m py_compile scripts/*.py
uv run python -m unittest discover -s scripts -p 'test_*.py'

CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target} \
  "$ROOT/scripts/cargo-local-1.99.sh" check --locked --features kv-all --bin kvbench
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target} \
  "$ROOT/scripts/cargo-local-1.99.sh" check --locked --features kv-all --bin kvconcurrency
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target} \
  "$ROOT/scripts/cargo-local-1.99.sh" check --locked --features kv-all --bin kvsustained
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target} \
  "$ROOT/scripts/cargo-local-1.99.sh" check --locked --features record --bin recordbench
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target} \
  "$ROOT/scripts/cargo-local-1.99.sh" check --locked --features record --bin recordconcurrency
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target} \
  "$ROOT/scripts/cargo-local-1.99.sh" check --locked --features record --bin recordsustained
"$ROOT/scripts/cargo-local-1.99.sh" fmt \
  --manifest-path "$ROOT/engines/tsdb-server/Cargo.toml" -- --check
"$ROOT/scripts/cargo-local-1.99.sh" check --locked \
  --manifest-path "$ROOT/engines/tsdb-server/Cargo.toml" \
  --target-dir "${TSDB_TARGET_DIR:-/tmp/rust-db-tsdb-target}"
"$ROOT/scripts/cargo-local-1.99.sh" test --locked \
  --manifest-path "$ROOT/engines/tsdb-server/Cargo.toml" \
  --target-dir "${TSDB_TARGET_DIR:-/tmp/rust-db-tsdb-target}"
"$ROOT/scripts/cargo-local-1.99.sh" clippy --locked \
  --manifest-path "$ROOT/engines/tsdb-server/Cargo.toml" \
  --target-dir "${TSDB_TARGET_DIR:-/tmp/rust-db-tsdb-target}" -- -D warnings
"$ROOT/scripts/cargo-local-1.99.sh" check --locked \
  --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" \
  --target-dir "${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}"
"$ROOT/scripts/cargo-local-1.99.sh" clippy --locked \
  --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" \
  --target-dir "${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}" -- -D warnings

echo "static validation passed"
