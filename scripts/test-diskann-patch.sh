#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"
host=$(rustc -vV | sed -n 's/^host: //p')
cargo metadata --locked --format-version 1 --filter-platform "$host" --features record |
    jq -e '
      [.packages[] | select(.name == "surrealdb-diskann")] |
      length == 1 and .[0].version == "0.61.0" and
      (.[0].source | startswith("git+https://github.com/codeandsolder/cargo-patches.git?tag=surrealdb-diskann-v0.0.1#"))
    ' >/dev/null
