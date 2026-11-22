#!/usr/bin/env bash
# Sourced by the pinning commands. Only published binaries/manifest survive.
set -euo pipefail
if [[ $# != 1 || -z "$1" ]]; then
  printf 'Usage: %s OUTPUT_DIRECTORY\n' "$0" >&2
  exit 64
fi
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
if [[ -n "$(git -C "$ROOT" status --porcelain=v1 --untracked-files=normal)" ]]; then
  printf 'Refusing to pin a dirty source checkout\n' >&2
  exit 2
fi
OUT=$(realpath -m -- "$1")
[[ ! -e "$OUT" && -d "$(dirname "$OUT")" ]] || {
  printf 'Output must be a new directory under an existing parent: %s\n' "$OUT" >&2
  exit 64
}
# Keep target trees private to this invocation, using Garage-backed sccache
# rather than sharing Cargo build artifacts across concurrent pinning runs.
build_root="${RUNNER_TEMP:-$HOME/build}"
mkdir -p "$build_root"
PIN_BUILD_ROOT=$(mktemp -d "$build_root/all-db-pin.XXXXXXXX")
PIN_OUTPUT="$OUT"
OUT=$(mktemp -d "$(dirname "$PIN_OUTPUT")/.all-db-pin.XXXXXXXX")
TARGET_DIR="$PIN_BUILD_ROOT/target"
ROCKS_TARGET_DIR="$PIN_BUILD_ROOT/rocks-target"
cleanup_pin_build() {
  rm -rf -- "$PIN_BUILD_ROOT"
  if [[ -n "${OUT:-}" && "$OUT" != "$PIN_OUTPUT" ]]; then
    rm -rf -- "$OUT"
  fi
}
trap cleanup_pin_build EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# Native compiler/cache policy is maintained by cargo-local-1.99.sh.
# Manifest paths must refer to their final published location, not staging.
publish_pin_build() {
  jq --arg stage "$OUT/" --arg final "$PIN_OUTPUT/" \
    'walk(if type == "string" and startswith($stage) then $final + ltrimstr($stage) else . end)' \
    "$OUT/manifest.json" > "$OUT/manifest.final.json"
  mv -- "$OUT/manifest.final.json" "$OUT/manifest.json"
  mv -T -- "$OUT" "$PIN_OUTPUT"
  OUT="$PIN_OUTPUT"
  cat "$OUT/manifest.json"
}
