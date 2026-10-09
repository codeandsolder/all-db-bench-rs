#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=sccache-native-common.sh
source "$SCRIPT_DIR/sccache-native-common.sh"
exec "$SCCACHE_BIN" "$@"
