#!/usr/bin/env bash
set -euo pipefail

ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
OUT=${1:-/srv/scratch/db-bench-work/concurrency-quick/bin}
mkdir -p "$OUT"
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
ROCKS_TARGET_DIR=${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}

CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" \
  build --release --locked --features kv-all --bin kvconcurrency
CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" \
  build --release --locked --features record --bin recordconcurrency
"$ROOT/scripts/cargo-local-1.99.sh" build --release --locked \
  --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" \
  --target-dir "$ROCKS_TARGET_DIR" --bin surrealdb-rocksdb-recordconcurrency

commit=$(git -C "$ROOT" rev-parse HEAD)
short=${commit:0:16}
kv="$OUT/kvconcurrency-$short"
record="$OUT/recordconcurrency-$short"
rocks="$OUT/surrealdb-rocksdb-recordconcurrency-$short"
install -m 0755 "$TARGET_DIR/release/kvconcurrency" "$kv"
install -m 0755 "$TARGET_DIR/release/recordconcurrency" "$record"
install -m 0755 "$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordconcurrency" "$rocks"
kv_sha=$(sha256sum "$kv" | awk '{print $1}')
record_sha=$(sha256sum "$record" | awk '{print $1}')
rocks_sha=$(sha256sum "$rocks" | awk '{print $1}')
jq -nS \
  --arg repo_commit "$commit" \
  --arg kv_path "$kv" --arg kv_sha "$kv_sha" \
  --arg record_path "$record" --arg record_sha "$record_sha" \
  --arg rocks_path "$rocks" --arg rocks_sha "$rocks_sha" \
  '{repo_commit:$repo_commit,kv:{path:$kv_path,sha256:$kv_sha},record:{path:$record_path,sha256:$record_sha},rocks:{path:$rocks_path,sha256:$rocks_sha}}' > "$OUT/manifest.json"
cat "$OUT/manifest.json"
