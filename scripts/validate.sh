#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

RUSTC_REAL=$(rustup which rustc --toolchain 1.98.1)
"$RUSTC_REAL" --version | rg '^rustc 1\.98\.1 '
"$ROOT/scripts/cargo-local-1.98.1.sh" --version
rg --version | head -1
uv --version
"$ROOT/scripts/ensure-sqlite-3.53.4.sh"

"$ROOT/scripts/cargo-local-1.98.1.sh" fmt --all --check
rustfmt +1.98.1 --edition 2024 engines/surrealdb-rocksdb/src/main.rs engines/surrealdb-rocksdb/src/metrics.rs --check
bash -n scripts/*.sh
uv run python -m py_compile scripts/summarize.py scripts/summarize-recovery.py
python3 -m py_compile scripts/cpu-pressure-worker.py

CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target} \
  "$ROOT/scripts/cargo-local-1.98.1.sh" check --features kv-all --bin kvbench
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target} \
  "$ROOT/scripts/cargo-local-1.98.1.sh" check --features kv-all --bin kvconcurrency
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target} \
  "$ROOT/scripts/cargo-local-1.98.1.sh" check --features record --bin recordbench
"$ROOT/scripts/cargo-local-1.98.1.sh" check \
  --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" \
  --target-dir "${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}"

echo "static validation passed"
