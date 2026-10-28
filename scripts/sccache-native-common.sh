#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SCCACHE_BIN=${SCCACHE_BIN:-$(command -v sccache || true)}
if [[ -z "$SCCACHE_BIN" || ! -x "$SCCACHE_BIN" ]]; then
  echo "sccache is required for benchmark builds; refusing uncached compilation" >&2
  exit 127
fi

if [[ -n "${DB_BENCH_SCCACHE_PORT:-}" && -n "${DB_BENCH_SCCACHE_UDS:-}" ]]; then
  echo "set only one of DB_BENCH_SCCACHE_PORT or DB_BENCH_SCCACHE_UDS" >&2
  exit 64
fi

if [[ -n "${DB_BENCH_SCCACHE_UDS:-}" ]]; then
  export SCCACHE_SERVER_UDS="$DB_BENCH_SCCACHE_UDS"
  unset SCCACHE_SERVER_PORT || true
elif [[ -n "${DB_BENCH_SCCACHE_PORT:-}" ]]; then
  export SCCACHE_SERVER_PORT="$DB_BENCH_SCCACHE_PORT"
  unset SCCACHE_SERVER_UDS || true
elif (( EUID == 0 )); then
  # Root builds use the root-owned managed daemon.
  export SCCACHE_SERVER_PORT=4237
  unset SCCACHE_SERVER_UDS || true
else
  # Non-root builds must use a same-owner daemon. A root-owned TCP daemon can
  # create native compiler outputs that the invoking user cannot read/archive.
  SCCACHE_USER=$(id -un)
  export SCCACHE_SERVER_UDS="/run/sccache-${SCCACHE_USER}/sccache.sock"
  unset SCCACHE_SERVER_PORT || true
fi

# Managed daemons own their scheduler/cache configuration. Clients must never
# auto-spawn a differently configured server or fall back to direct compilation.
export SCCACHE_START_SERVER="${DB_BENCH_SCCACHE_START_SERVER:-0}"
export SCCACHE_CONF="${DB_BENCH_NATIVE_SCCACHE_CONF:-$SCRIPT_DIR/sccache-native-local.conf}"
unset SCCACHE_EXPERIMENTAL_CANONICAL_RUST || true
