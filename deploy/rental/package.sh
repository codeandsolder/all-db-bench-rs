#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
OUTPUT="$ROOT/target/rental-bundle"
BIN_DIR="$ROOT/target/rental-release/release"
ROCKS_BIN="$ROOT/target/rental-rocks/release/surrealdb-rocksdb-recordbench"
WITH_TSDB=0

usage() {
  echo "usage: deploy/rental/package.sh [--output DIRECTORY] [--bin-dir DIRECTORY] [--rocks-bin FILE] [--with-tsdb]" >&2
}
while (($#)); do
  case "$1" in
    --output|--bin-dir|--rocks-bin)
      (($# >= 2)) || { usage; exit 2; }
      case "$1" in --output) OUTPUT=$2 ;; --bin-dir) BIN_DIR=$2 ;; --rocks-bin) ROCKS_BIN=$2 ;; esac
      shift 2 ;;
    --with-tsdb) WITH_TSDB=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage; exit 2 ;;
  esac
done
[[ "$OUTPUT" == /* ]] || { echo "output must be absolute" >&2; exit 2; }
[[ ! -e "$OUTPUT" ]] || { echo "bundle output already exists: $OUTPUT" >&2; exit 2; }
git -C "$ROOT" diff --quiet && git -C "$ROOT" diff --cached --quiet || {
  echo "refusing to package uncommitted tracked source changes" >&2; exit 2;
}
source_sha=$(git -C "$ROOT" rev-parse HEAD)
[[ "$source_sha" =~ ^[0-9a-f]{40}$ ]] || exit 2
mkdir -p "$(dirname "$OUTPUT")"
temp=$(mktemp -d "$(dirname "$OUTPUT")/.rental-bundle.XXXXXXXX")
trap 'rm -rf -- "$temp"' EXIT
git clone --quiet --no-hardlinks -- "$ROOT" "$temp/source"
git -C "$temp/source" checkout --quiet --detach "$source_sha"
git -C "$temp/source" remote remove origin || true
mkdir -p "$temp/source/.rental/bin"
binaries=(kvbench recordbench all-db-benchctl surrealdb-rocksdb-recordbench)
((WITH_TSDB == 0)) || binaries+=(tsdb-server-bench)
hashes='{}'
for name in "${binaries[@]}"; do
  if [[ "$name" == surrealdb-rocksdb-recordbench ]]; then file=$ROCKS_BIN; else file="$BIN_DIR/$name"; fi
  [[ -x "$file" ]] || { echo "missing build artifact: $file" >&2; exit 2; }
  cp -- "$file" "$temp/source/.rental/bin/$name"
  hash=$(sha256sum "$temp/source/.rental/bin/$name" | awk '{print $1}')
  hashes=$(jq -c --arg name "$name" --arg hash "$hash" '. + {($name):$hash}' <<< "$hashes")
done
if ((WITH_TSDB)); then
  src="$ROOT/.deps/tsdb"
  [[ -d "$src" ]] || { echo "missing TSDB .deps: $src" >&2; exit 2; }
  mkdir -p "$temp/source/.deps/tsdb"
  cp -a -- "$src/." "$temp/source/.deps/tsdb/"
fi
glibc=$(ldd --version | sed -n '1p')
jq -n --arg sha "$source_sha" --arg glibc "$glibc" --arg target "x86-64-v2+aes" \
  --argjson binaries "$hashes" '{format_version:1,source_commit:$sha,compile_target:$target,build_libc:$glibc,record_sqlite_version:"3.54.0",record_sqlite_linkage:"static",binaries:$binaries}' \
  > "$temp/source/.rental/manifest.json"
git -C "$temp/source" status --short --untracked-files=no | while IFS= read -r line; do
  [[ -z "$line" ]] || { echo "unexpected source mutation during package: $line" >&2; exit 2; }
done
mv -- "$temp/source" "$OUTPUT"
echo "Rental image build context ready: $OUTPUT"
echo "Build: docker build -f $ROOT/deploy/rental/Dockerfile -t all-db-bench:$source_sha $OUTPUT"
