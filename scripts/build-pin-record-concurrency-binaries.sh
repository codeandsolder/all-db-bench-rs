#!/usr/bin/env bash
set -euo pipefail
ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
OUT=${1:-/srv/scratch/db-bench-work/record-concurrency-steady/bin}
mkdir -p "$OUT"
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
ROCKS_TARGET_DIR=${SURREAL_ROCKS_TARGET_DIR:-/tmp/rust-db-surreal-rocks-target}

CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features record --bin recordconcurrency
"$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --manifest-path "$ROOT/engines/surrealdb-rocksdb/Cargo.toml" --target-dir "$ROCKS_TARGET_DIR" --bin surrealdb-rocksdb-recordconcurrency

commit=$(git -C "$ROOT" rev-parse HEAD)
short=${commit:0:16}
record="$OUT/recordconcurrency-$short"
rocks="$OUT/surrealdb-rocksdb-recordconcurrency-$short"
install -m 0755 "$TARGET_DIR/release/recordconcurrency" "$record"
install -m 0755 "$ROCKS_TARGET_DIR/release/surrealdb-rocksdb-recordconcurrency" "$rocks"
record_sha=$(sha256sum "$record" | awk '{print $1}')
rocks_sha=$(sha256sum "$rocks" | awk '{print $1}')
python3 - "$OUT/manifest.json" <<PY
import json,sys
json.dump({
  "repo_commit":"$commit",
  "record":{"path":"$record","sha256":"$record_sha"},
  "rocks":{"path":"$rocks","sha256":"$rocks_sha"},
},open(sys.argv[1],"w"),indent=2,sort_keys=True)
open(sys.argv[1],"a").write("\n")
PY
cat "$OUT/manifest.json"
