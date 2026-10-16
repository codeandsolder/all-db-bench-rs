#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=${ROOT:-$(cd -- "$SCRIPT_DIR/.." && pwd)}
VERSION=3.54.0
SOURCE_ID=be8d059e9a49089ab2dce5ed26dd87aaf598fdc5fbe0b107c0dd758464bcd5e3
AUTOCONF=3540000
EXPECTED_SHA3=f5e3e724e9c9b3b5d512c76b02082091a59eeea9becd169a9fbf4d376403acce
URL="https://www.sqlite.org/2026/sqlite-autoconf-${AUTOCONF}.tar.gz"
PREFIX="$ROOT/.deps/sqlite-$VERSION"
SRC_CACHE="$ROOT/.deps/src"
TARBALL="$SRC_CACHE/sqlite-autoconf-${AUTOCONF}.tar.gz"
BUILD_ROOT="${SQLITE_BUILD_ROOT:-/tmp/db-bench-sqlite-build}"
JOBS="${SQLITE_BUILD_JOBS:-1}"

valid_install() {
  [[ -x "$PREFIX/bin/sqlite3" ]] || return 1
  [[ -f "$PREFIX/lib/libsqlite3.a" ]] || return 1
  [[ -f "$PREFIX/include/sqlite3.h" ]] || return 1
  read -r actual_version actual_source_id < <("$PREFIX/bin/sqlite3" --version | awk '{print $1, $4}')
  [[ "$actual_version" == "$VERSION" && "$actual_source_id" == "$SOURCE_ID" ]]
}

if valid_install; then
  echo "SQLite $VERSION already installed at $PREFIX" >&2
  exit 0
fi

mkdir -p "$SRC_CACHE"
if [[ ! -f "$TARBALL" ]]; then
  tmp="$TARBALL.part.$$"
  trap 'rm -f "$tmp"' EXIT
  curl -fL --retry 3 --retry-delay 2 -o "$tmp" "$URL"
  mv "$tmp" "$TARBALL"
  trap - EXIT
fi

actual_sha3=$(uv run python - "$TARBALL" <<'PY'
import hashlib
import pathlib
import sys

p = pathlib.Path(sys.argv[1])
h = hashlib.sha3_256()
with p.open("rb") as f:
    for chunk in iter(lambda: f.read(1024 * 1024), b""):
        h.update(chunk)
print(h.hexdigest())
PY
)
if [[ "$actual_sha3" != "$EXPECTED_SHA3" ]]; then
  echo "SQLite source SHA3-256 mismatch" >&2
  echo "expected: $EXPECTED_SHA3" >&2
  echo "actual:   $actual_sha3" >&2
  exit 65
fi

rm -rf "$BUILD_ROOT"
mkdir -p "$BUILD_ROOT"
tar -xzf "$TARBALL" -C "$BUILD_ROOT"
SRC="$BUILD_ROOT/sqlite-autoconf-${AUTOCONF}"
[[ -x "$SRC/configure" ]] || { echo "SQLite configure script missing after extraction" >&2; exit 66; }

rm -rf "$PREFIX"
cd "$SRC"
./configure --prefix="$PREFIX" --disable-shared --enable-static CFLAGS="-O2 -g0"
make -j"$JOBS"
make install

if ! valid_install; then
  echo "SQLite $VERSION install verification failed" >&2
  exit 1
fi

echo "SQLite $VERSION installed and verified at $PREFIX" >&2
echo "source_sha3_256=$actual_sha3" >&2
