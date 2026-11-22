#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=pin-build-common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/pin-build-common.sh" "$@"

CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --features kv-all --bin kvbench
commit=$(git -C "$ROOT" rev-parse HEAD)
short=${commit:0:16}
kv="$OUT/kvbench-$short"
install -m 0755 "$TARGET_DIR/release/kvbench" "$kv"
kv_sha=$(sha256sum "$kv" | awk '{print $1}')
jq -nS \
  --arg repo_commit "$commit" --arg path "$kv" --arg sha256 "$kv_sha" \
  '{repo_commit:$repo_commit,kv:{path:$path,sha256:$sha256}}' > "$OUT/manifest.json"
publish_pin_build
