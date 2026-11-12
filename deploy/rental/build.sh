#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
WITH_TSDB=0
[[ "${1:-}" != --with-tsdb ]] || { WITH_TSDB=1; shift; }
(($# == 0)) || { echo "usage: deploy/rental/build.sh [--with-tsdb]" >&2; exit 2; }
# Deliberately builds a generic x86-64-v2 image variant, not host-native binaries
# that would SIGILL on a different CPU. Compilation must use the project sccache.
export RUSTFLAGS='-C target-cpu=x86-64-v2 -C target-feature=+aes,+sse2'
export CFLAGS='-march=x86-64-v2 -maes'
export CXXFLAGS='-march=x86-64-v2 -maes'
export CARGO_PROFILE_RELEASE_STRIP=debuginfo
CARGO_TARGET_DIR="$ROOT/target/rental-release"
ROCKS_TARGET_DIR="$ROOT/target/rental-rocks"
export CARGO_TARGET_DIR
mkdir -p "$CARGO_TARGET_DIR" "$ROCKS_TARGET_DIR"
# Record comparisons require the verified SQLite implementation, not a
# distribution-dependent system libsqlite build. Compile it through sccache.
SQLITE_BUILD_ROOT="$ROOT/target/rental-sqlite-build" \
  SQLITE_BUILD_CFLAGS="-O2 -g0 -march=x86-64-v2 -maes" \
  CC="$ROOT/scripts/sccache-native-cc.sh" \
  "$ROOT/scripts/ensure-sqlite-3.54.0.sh"
"$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features kv-all --bin kvbench
"$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features record --bin recordbench
"$ROOT/scripts/cargo-local-1.99.sh" build --release --locked \
  --manifest-path "$ROOT/tools/benchctl/Cargo.toml" --bin all-db-benchctl
"$ROOT/scripts/cargo-local-1.99.sh" build --release --locked \
  --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" \
  --target-dir "$ROCKS_TARGET_DIR" --bin surrealdb-rocksdb-recordbench
if ((WITH_TSDB)); then
  "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked \
    --manifest-path "$ROOT/engines/tsdb-server/Cargo.toml" --bin tsdb-server-bench
  "$ROOT/scripts/ensure-tsdb-binaries.sh"
fi
echo "Rental binaries ready under $ROOT/target; package with deploy/rental/package.sh"
