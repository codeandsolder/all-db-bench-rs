#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=${ROOT:-$(cd -- "$SCRIPT_DIR/.." && pwd)}
COMMIT=7b70d8a6863c5de30933d42a7672d35d01d2dc6c
SHORT=${COMMIT:0:12}
SRC="$ROOT/.deps/log-writes-$SHORT"
URL=https://github.com/josefbacik/log-writes

valid() {
  [[ -x "$SRC/replay-log" ]] || return 1
  [[ "$(git -C "$SRC" rev-parse HEAD 2>/dev/null)" == "$COMMIT" ]]
}

if ! valid; then
  command -v git >/dev/null
  command -v make >/dev/null
  command -v gcc >/dev/null
  rm -rf "$SRC"
  mkdir -p "$ROOT/.deps"
  git init -q "$SRC"
  git -C "$SRC" remote add origin "$URL"
  git -C "$SRC" fetch -q --depth 1 origin "$COMMIT"
  git -C "$SRC" checkout -q --detach FETCH_HEAD
  make -C "$SRC" replay-log >/dev/null
fi

valid || { echo "failed to build pinned replay-log $COMMIT" >&2; exit 1; }
printf '%s\n' "$SRC/replay-log"
