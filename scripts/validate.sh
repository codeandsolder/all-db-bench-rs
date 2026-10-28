#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

RUSTC_REAL=$(rustup which rustc --toolchain 1.99.1)
"$RUSTC_REAL" --version | rg '^rustc 1\.99\.1 '
"$ROOT/scripts/cargo-local-1.99.sh" --version
rg --version | head -1
jq --version
fio --version
"$ROOT/scripts/ensure-sqlite-3.54.0.sh"

"$ROOT/scripts/cargo-local-1.99.sh" metadata --locked --no-deps --format-version 1 >/dev/null
"$ROOT/scripts/cargo-local-1.99.sh" fmt --all --check
"$ROOT/scripts/cargo-local-1.99.sh" fmt \
  --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" -- --check
bash -n scripts/*.sh
for test_script in "$ROOT"/scripts/test-*.sh; do
  "$test_script"
done

ROOT_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
CARGO_TARGET_DIR="$ROOT_TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" test --locked \
  --workspace --all-targets
CARGO_TARGET_DIR="$ROOT_TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" clippy --locked \
  --workspace --all-targets -- -D warnings

CARGO_TARGET_DIR="$ROOT_TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" test --locked \
  -p rust-db-realistic-bench --all-targets --features record
CARGO_TARGET_DIR="$ROOT_TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" clippy --locked \
  -p rust-db-realistic-bench --all-targets --features record -- -D warnings
CARGO_TARGET_DIR="$ROOT_TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" test --locked \
  -p rust-db-realistic-bench --all-targets --features kv-all
CARGO_TARGET_DIR="$ROOT_TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" clippy --locked \
  -p rust-db-realistic-bench --all-targets --features kv-all -- -D warnings

SURREAL_ROCKS_TARGET_DIR=${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}
"$ROOT/scripts/cargo-local-1.99.sh" test --locked \
  --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" --all-targets \
  --target-dir "$SURREAL_ROCKS_TARGET_DIR"
"$ROOT/scripts/cargo-local-1.99.sh" clippy --locked \
  --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" --all-targets \
  --target-dir "$SURREAL_ROCKS_TARGET_DIR" -- -D warnings

echo "static validation passed"
