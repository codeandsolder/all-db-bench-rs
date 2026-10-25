#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
TARGET_DIR=${BENCHCTL_TARGET_DIR:-${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}}
BIN=${BENCHCTL_BIN:-$TARGET_DIR/debug/all-db-benchctl}
if [[ -z "${BENCHCTL_BIN:-}" ]]; then
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --locked \
    --manifest-path "$ROOT/tools/benchctl/Cargo.toml" --bin all-db-benchctl
fi
exec "$BIN" "$@"
