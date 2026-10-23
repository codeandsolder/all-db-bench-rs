#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

FSTYPE=$(findmnt -T "$TMP" -n -o FSTYPE)
SOURCE=$(findmnt -T "$TMP" -n -o SOURCE)
HOST=$(hostname)

cat >"$TMP/native.json" <<JSON
{"format_version":1,"id":"test-native","mode":"native","root":"$TMP","expected":{"hostname":"$HOST","filesystem":"$FSTYPE","source":"$SOURCE"}}
JSON

# shellcheck disable=SC2016
out=$("$ROOT/scripts/run-storage-envelope.sh" "$TMP/native.json" -- sh -c 'printf "%s %s\n" "$DBBENCH_STORAGE_PROFILE_ID" "$DBBENCH_STORAGE_ROOT"')
[[ "$out" == "test-native $TMP" ]] || { echo "unexpected native wrapper output: $out" >&2; exit 1; }

jq '.expected.hostname = "definitely-not-this-host.invalid"' "$TMP/native.json" >"$TMP/bad.json"
set +e
"$ROOT/scripts/run-storage-envelope.sh" "$TMP/bad.json" -- true >/dev/null 2>&1
rc=$?
set -e
[[ $rc -eq 75 ]] || { echo "expected host mismatch exit 75, got $rc" >&2; exit 1; }

echo "storage envelope tests passed"
