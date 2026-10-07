#!/usr/bin/env bash
set -euo pipefail
ROOT=${ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
OUT=${1:-/srv/scratch/db-bench-work/reopen-quick/bin}
mkdir -p "$OUT"
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features kv-all --bin kvbench
commit=$(git -C "$ROOT" rev-parse HEAD)
short=${commit:0:16}
kv="$OUT/kvbench-$short"
install -m 0755 "$TARGET_DIR/release/kvbench" "$kv"
kv_sha=$(sha256sum "$kv" | awk '{print $1}')
python3 - "$OUT/manifest.json" <<PY
import json,sys
json.dump({"repo_commit":"$commit","kv":{"path":"$kv","sha256":"$kv_sha"}},open(sys.argv[1],"w"),indent=2,sort_keys=True)
open(sys.argv[1],"a").write("\n")
PY
cat "$OUT/manifest.json"
