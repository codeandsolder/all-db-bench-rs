#!/usr/bin/env bash
set -euo pipefail

[[ $# -eq 1 ]] || { echo "usage: $0 PATH" >&2; exit 2; }
PATH_TO_SETTLE=$1
[[ -e "$PATH_TO_SETTLE" ]] || { echo "storage settle path does not exist: $PATH_TO_SETTLE" >&2; exit 75; }

for tool in findmnt sync; do
  command -v "$tool" >/dev/null 2>&1 || { echo "required storage-settle tool not found: $tool" >&2; exit 127; }
done

FSTYPE=$(findmnt -T "$PATH_TO_SETTLE" -n -o FSTYPE 2>/dev/null || true)
SOURCE=$(findmnt -T "$PATH_TO_SETTLE" -n -o SOURCE 2>/dev/null || true)
[[ -n "$FSTYPE" && -n "$SOURCE" ]] || {
  echo "could not resolve storage filesystem/source for: $PATH_TO_SETTLE" >&2
  exit 75
}

if [[ "$FSTYPE" == zfs ]]; then
  command -v zpool >/dev/null 2>&1 || { echo "zpool is required to settle ZFS storage" >&2; exit 127; }
  POOL=${SOURCE%%/*}
  [[ -n "$POOL" ]] || { echo "could not derive ZFS pool from source: $SOURCE" >&2; exit 75; }
  zpool sync "$POOL"
else
  # sync -f issues syncfs(2) for the filesystem containing PATH_TO_SETTLE.
  sync -f "$PATH_TO_SETTLE"
fi
