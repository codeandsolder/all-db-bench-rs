#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/fake-sccache" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
printf 'port=%s\n' "${SCCACHE_SERVER_PORT:-}"
printf 'start=%s\n' "${SCCACHE_START_SERVER-unset}"
printf 'conf=%s\n' "${SCCACHE_CONF:-}"
printf 'canonical=%s\n' "${SCCACHE_EXPERIMENTAL_CANONICAL_RUST-unset}"
printf 'argv='
printf '<%s>' "$@"
printf '\n'
FAKE
chmod 0755 "$TMP/fake-sccache"

out=$(
  SCCACHE_BIN="$TMP/fake-sccache" \
  DB_BENCH_SCCACHE_PORT=4999 \
  "$ROOT/scripts/sccache-native-rustc.sh" /toolchains/rustc --crate-name demo
)
grep -Fx 'port=4999' <<<"$out"
grep -Fx 'start=unset' <<<"$out"
grep -Fx "conf=$ROOT/scripts/sccache-native-local.conf" <<<"$out"
grep -Fx 'canonical=unset' <<<"$out"
grep -Fx 'argv=</toolchains/rustc><--crate-name><demo>' <<<"$out"

out=$(
  SCCACHE_BIN="$TMP/fake-sccache" \
  DB_BENCH_CC_REAL=/usr/bin/cc \
  "$ROOT/scripts/sccache-native-cc.sh" -c demo.c
)
grep -Fx 'canonical=unset' <<<"$out"
grep -Fx 'argv=</usr/bin/cc><-c><demo.c>' <<<"$out"

out=$(
  SCCACHE_BIN="$TMP/fake-sccache" \
  DB_BENCH_CXX_REAL=/usr/bin/c++ \
  "$ROOT/scripts/sccache-native-cxx.sh" -c demo.cc
)
grep -Fx 'canonical=unset' <<<"$out"
grep -Fx 'argv=</usr/bin/c++><-c><demo.cc>' <<<"$out"

if SCCACHE_BIN="$TMP/does-not-exist" "$ROOT/scripts/sccache-native-rustc.sh" /toolchains/rustc >/dev/null 2>"$TMP/missing.err"; then
  echo "expected missing sccache to fail closed" >&2
  exit 1
fi
grep -F 'refusing uncached compilation' "$TMP/missing.err"

echo 'native sccache wrapper tests passed'
