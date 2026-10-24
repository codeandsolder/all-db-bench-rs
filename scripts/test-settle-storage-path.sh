#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/path"
LOG="$TMP/calls.log"

cat > "$TMP/bin/findmnt" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
out=
while (( $# )); do
  if [[ "$1" == -o ]]; then out=$2; shift 2; else shift; fi
done
case "$out" in
  FSTYPE) printf '%s\n' "$FAKE_FSTYPE" ;;
  SOURCE) printf '%s\n' "$FAKE_SOURCE" ;;
  *) exit 2 ;;
esac
FAKE
cat > "$TMP/bin/zpool" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
printf 'zpool' >> "$FAKE_LOG"
printf ' <%s>' "$@" >> "$FAKE_LOG"
printf '\n' >> "$FAKE_LOG"
FAKE
cat > "$TMP/bin/sync" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
printf 'sync' >> "$FAKE_LOG"
printf ' <%s>' "$@" >> "$FAKE_LOG"
printf '\n' >> "$FAKE_LOG"
FAKE
chmod 0755 "$TMP/bin/"*

: > "$LOG"
PATH="$TMP/bin:$PATH" FAKE_LOG="$LOG" FAKE_FSTYPE=zfs FAKE_SOURCE=zpcachyos/dbbench/off-all \
  "$ROOT/scripts/settle-storage-path.sh" "$TMP/path"
grep -Fx 'zpool <sync> <zpcachyos>' "$LOG"
if grep -q '^sync ' "$LOG"; then exit 1; fi

: > "$LOG"
PATH="$TMP/bin:$PATH" FAKE_LOG="$LOG" FAKE_FSTYPE=ext4 FAKE_SOURCE=/dev/nvme0n1p1 \
  "$ROOT/scripts/settle-storage-path.sh" "$TMP/path"
grep -Fx "sync <-f> <$TMP/path>" "$LOG"
if grep -q '^zpool ' "$LOG"; then exit 1; fi

echo 'storage settle tests passed'
