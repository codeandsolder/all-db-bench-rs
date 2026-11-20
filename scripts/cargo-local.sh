#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
if [[ -z "${CARGO_REAL:-}" ]]; then
  CARGO_REAL=$(rustup which cargo --toolchain 1.100.0 2>/dev/null || true)
fi
if [[ -z "$CARGO_REAL" || ! -x "$CARGO_REAL" ]]; then
  echo "Cargo binary not found (set CARGO_REAL explicitly)" >&2
  exit 127
fi
RUSTC_REAL=${RUSTC_REAL:-$(rustup which rustc --toolchain 1.100.0)}
if [[ ! -x "$RUSTC_REAL" ]]; then
  echo "Rust 1.100.0 rustc not found: $RUSTC_REAL" >&2
  exit 127
fi
export RUSTUP_TOOLCHAIN=1.100.0
export RUSTC="$RUSTC_REAL"
# shellcheck source=sccache-native-common.sh
source "$ROOT/scripts/sccache-native-common.sh"
if [[ -n "${SCCACHE_SERVER_UDS:-}" ]]; then
  SCCACHE_ENDPOINT="UDS $SCCACHE_SERVER_UDS"
else
  SCCACHE_ENDPOINT="port ${SCCACHE_SERVER_PORT:-unset}"
fi
"$SCCACHE_BIN" --show-stats >/dev/null 2>&1 || {
  echo "native sccache server at $SCCACHE_ENDPOINT is unavailable; refusing uncached build" >&2
  exit 70
}
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
export CARGO_HOME="${DB_BENCH_CARGO_HOME:-${CARGO_HOME:-$HOME/.cargo}}"
mkdir -p "$CARGO_HOME"
SQLITE_PREFIX="$ROOT/.deps/sqlite-3.54.0"
if [[ -f "$SQLITE_PREFIX/lib/libsqlite3.a" && -f "$SQLITE_PREFIX/include/sqlite3.h" ]]; then
  export SQLITE3_LIB_DIR="$SQLITE_PREFIX/lib"
  export SQLITE3_INCLUDE_DIR="$SQLITE_PREFIX/include"
  export SQLITE3_STATIC=1
fi
exec "$CARGO_REAL" "$@"
