#!/usr/bin/env bash
set -euo pipefail
ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
OUT=${1:-/srv/scratch/db-bench-work/sustained-quick/bin}
mkdir -p "$OUT"
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features kv-all --bin kvsustained
CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features record --bin recordsustained
commit=$(git -C "$ROOT" rev-parse HEAD)
short=${commit:0:16}
kv="$OUT/kvsustained-$short"
record="$OUT/recordsustained-$short"
install -m 0755 "$TARGET_DIR/release/kvsustained" "$kv"
install -m 0755 "$TARGET_DIR/release/recordsustained" "$record"
kv_sha=$(sha256sum "$kv" | awk '{print $1}')
record_sha=$(sha256sum "$record" | awk '{print $1}')
python3 - "$OUT/manifest.json" <<PY
import json,sys
json.dump({
  "repo_commit":"$commit",
  "kv":{"path":"$kv","sha256":"$kv_sha"},
  "record":{"path":"$record","sha256":"$record_sha"},
},open(sys.argv[1],"w"),indent=2,sort_keys=True)
open(sys.argv[1],"a").write("\n")
PY
cat "$OUT/manifest.json"
