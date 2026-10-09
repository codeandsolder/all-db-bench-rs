#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SCCACHE_BIN=${SCCACHE_BIN:-$(command -v sccache || true)}
if [[ -z "$SCCACHE_BIN" || ! -x "$SCCACHE_BIN" ]]; then
  echo "sccache is required for benchmark builds; refusing uncached compilation" >&2
  exit 127
fi

export SCCACHE_SERVER_PORT="${DB_BENCH_SCCACHE_PORT:-4237}"
# The benchmark cache daemon is a managed local-only service. Clients must
# never auto-spawn a differently configured server if it is unavailable.
export SCCACHE_START_SERVER="${DB_BENCH_SCCACHE_START_SERVER:-0}"
export SCCACHE_CONF="${DB_BENCH_NATIVE_SCCACHE_CONF:-$SCRIPT_DIR/sccache-native-local.conf}"
unset SCCACHE_EXPERIMENTAL_CANONICAL_RUST || true
