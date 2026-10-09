#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=sccache-native-common.sh
source "$SCRIPT_DIR/sccache-native-common.sh"
REAL_CXX=${DB_BENCH_CXX_REAL:-$(command -v c++ || true)}
if [[ -z "$REAL_CXX" || ! -x "$REAL_CXX" ]]; then
  echo "C++ compiler not found (set DB_BENCH_CXX_REAL)" >&2
  exit 127
fi
exec "$SCCACHE_BIN" "$REAL_CXX" "$@"
