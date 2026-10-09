#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
if [[ -z "${CARGO_REAL:-}" ]]; then
  if [[ -x /opt/cargo-ephemeral/current/cargo ]]; then
    CARGO_REAL=/opt/cargo-ephemeral/current/cargo
  else
    CARGO_REAL=$(rustup which cargo --toolchain 1.99.0 2>/dev/null || command -v cargo || true)
  fi
fi
if [[ -z "$CARGO_REAL" || ! -x "$CARGO_REAL" ]]; then
  echo "Cargo binary not found (set CARGO_REAL explicitly)" >&2
  exit 127
fi
RUSTC_REAL=${RUSTC_REAL:-$(rustup which rustc --toolchain 1.99.0)}
if [[ ! -x "$RUSTC_REAL" ]]; then
  echo "Rust 1.99.0 rustc not found: $RUSTC_REAL" >&2
  exit 127
fi
export RUSTUP_TOOLCHAIN=1.99.0
export RUSTC="$RUSTC_REAL"
NATIVE_SCCACHE_WRAPPER=${DB_BENCH_RUSTC_WRAPPER:-$ROOT/scripts/sccache-native-rustc.sh}
NATIVE_CC_WRAPPER=${DB_BENCH_CC:-$ROOT/scripts/sccache-native-cc.sh}
NATIVE_CXX_WRAPPER=${DB_BENCH_CXX:-$ROOT/scripts/sccache-native-cxx.sh}
for wrapper in "$NATIVE_SCCACHE_WRAPPER" "$NATIVE_CC_WRAPPER" "$NATIVE_CXX_WRAPPER"; do
  if [[ ! -x "$wrapper" ]]; then
    echo "Required native sccache wrapper is not executable: $wrapper" >&2
    exit 127
  fi
done
export RUSTC_WRAPPER="$NATIVE_SCCACHE_WRAPPER"
export CC="$NATIVE_CC_WRAPPER"
export CXX="$NATIVE_CXX_WRAPPER"
export CARGO_HOME="${DB_BENCH_CARGO_HOME:-/tmp/db-bench-cargo-home}"
mkdir -p "$CARGO_HOME"
SQLITE_PREFIX="$ROOT/.deps/sqlite-3.53.4"
if [[ -f "$SQLITE_PREFIX/lib/libsqlite3.a" && -f "$SQLITE_PREFIX/include/sqlite3.h" ]]; then
  export SQLITE3_LIB_DIR="$SQLITE_PREFIX/lib"
  export SQLITE3_INCLUDE_DIR="$SQLITE_PREFIX/include"
  export SQLITE3_STATIC=1
fi
exec "$CARGO_REAL" "$@"
