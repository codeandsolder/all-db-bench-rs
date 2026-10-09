#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=sccache-native-common.sh
source "$SCRIPT_DIR/sccache-native-common.sh"
REAL_CC=${DB_BENCH_CC_REAL:-/usr/bin/cc}
if [[ -z "$REAL_CC" || ! -x "$REAL_CC" ]]; then
  echo "C compiler not found (set DB_BENCH_CC_REAL)" >&2
  exit 127
fi
exec "$SCCACHE_BIN" "$REAL_CC" "$@"
