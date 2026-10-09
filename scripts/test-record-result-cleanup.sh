#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
# shellcheck source=record-result-cleanup.sh
source "$ROOT/scripts/record-result-cleanup.sh"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/data/case-db"
printf '%s\n' "{\"path\":\"$TMP/data/case-db\",\"db_kept\":true,\"value\":1}" > "$TMP/out.json"
record_cleanup_result_db "$TMP/out.json" "$TMP/data"
test ! -e "$TMP/data/case-db"
jq -e '.db_kept == false and .value == 1' "$TMP/out.json" >/dev/null
mkdir -p "$TMP/outside"
printf '%s\n' "{\"path\":\"$TMP/outside\",\"db_kept\":true}" > "$TMP/bad.json"
if record_cleanup_result_db "$TMP/bad.json" "$TMP/data" 2>/dev/null; then
  echo "expected cleanup outside data root to fail" >&2
  exit 1
fi
test -d "$TMP/outside"
echo 'record result cleanup tests passed'
