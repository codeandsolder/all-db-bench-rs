#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"
lock_hash=$(sha256sum Cargo.lock)
./scripts/prepare-diskann-patch.sh
source_dir="$ROOT/.deps/cargo-patches/surrealdb-diskann-0.61.0"
source_file="$source_dir/src/graph/index.rs"
before=$(stat -c '%i %y' "$source_file")
./scripts/prepare-diskann-patch.sh
test "$before" = "$(stat -c '%i %y' "$source_file")"
git -C "$source_dir" apply --reverse --check "$ROOT/patches/surrealdb-diskann-0.61.0.patch"
host=$(rustc -vV | sed -n 's/^host: //p')
cargo --config patches/cargo-config.toml metadata --locked --format-version 1 \
    --filter-platform "$host" --features record |
    jq -e --arg manifest "$source_dir/Cargo.toml" '
      [.packages[] | select(.name == "surrealdb-diskann")] |
      length == 1 and .[0].version == "0.61.0" and
      .[0].source == null and .[0].manifest_path == $manifest
    ' >/dev/null
test "$lock_hash" = "$(sha256sum Cargo.lock)"
