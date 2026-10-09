#!/usr/bin/env bash
set -euo pipefail

ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
OUT=${1:-/srv/scratch/db-bench-work/record-full-v1/bin}
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-record-full-v1-target}
ROCKS_TARGET_DIR=${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-record-full-v1-rocks-target}

if [[ -n "$(git -C "$ROOT" status --porcelain=v1 --untracked-files=normal)" ]]; then
  echo "refusing to pin benchmark binaries from a dirty checkout (tracked or untracked changes present)" >&2
  exit 2
fi

mkdir -p "$OUT"
CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build \
  --release --locked --features record \
  --bin recordbench --bin recordconcurrency --bin recordsustained
"$ROOT/scripts/cargo-local-1.99.sh" build \
  --release --locked \
  --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" \
  --target-dir "$ROCKS_TARGET_DIR" \
  --bin surrealdb-rocksdb-recordbench \
  --bin surrealdb-rocksdb-recordconcurrency \
  --bin surrealdb-rocksdb-recordsustained

commit=$(git -C "$ROOT" rev-parse HEAD)
short=${commit:0:16}
rustc_version=$(rustc +1.99.0 --version)

declare -A sources=(
  [recordbench]="$TARGET_DIR/release/recordbench"
  [recordconcurrency]="$TARGET_DIR/release/recordconcurrency"
  [recordsustained]="$TARGET_DIR/release/recordsustained"
  [surrealdb-rocksdb-recordbench]="$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordbench"
  [surrealdb-rocksdb-recordconcurrency]="$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordconcurrency"
  [surrealdb-rocksdb-recordsustained]="$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordsustained"
)

manifest_tmp="$OUT/manifest.json.tmp"
printf '{\n' > "$manifest_tmp"
printf '  "manifest_version": 1,\n' >> "$manifest_tmp"
printf '  "repo_commit": "%s",\n' "$commit" >> "$manifest_tmp"
printf '  "rustc_version": "%s",\n' "$rustc_version" >> "$manifest_tmp"
printf '  "target_cpu": "native",\n' >> "$manifest_tmp"
printf '  "read_materialization": "full-record-v1",\n' >> "$manifest_tmp"
printf '  "write_materialization": "no-return-v1",\n' >> "$manifest_tmp"
printf '  "build_cache": "sccache-local-only",\n' >> "$manifest_tmp"
printf '  "binaries": {\n' >> "$manifest_tmp"

names=(
  recordbench
  recordconcurrency
  recordsustained
  surrealdb-rocksdb-recordbench
  surrealdb-rocksdb-recordconcurrency
  surrealdb-rocksdb-recordsustained
)
for i in "${!names[@]}"; do
  name=${names[$i]}
  dst="$OUT/$name-$short"
  install -m 0755 "${sources[$name]}" "$dst"
  sha=$(sha256sum "$dst" | awk '{print $1}')
  comma=,
  if (( i == ${#names[@]} - 1 )); then comma=; fi
  printf '    "%s": {"path": "%s", "sha256": "%s"}%s\n' "$name" "$dst" "$sha" "$comma" >> "$manifest_tmp"
done
printf '  }\n}\n' >> "$manifest_tmp"

python3 -m json.tool "$manifest_tmp" >/dev/null
mv "$manifest_tmp" "$OUT/manifest.json"
cat "$OUT/manifest.json"
