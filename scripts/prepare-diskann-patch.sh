#!/usr/bin/env bash
# Materialize the exact published source outside the Cargo registry cache.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
crate=surrealdb-diskann-0.61.0
checksum=a953ad510f5c2ead708c8e5ed37df7dec7eb41198bbb6d7fb1d71174851e3511
patch="$ROOT/patches/$crate.patch"
base="$ROOT/.deps/cargo-patches"
dest="$base/$crate"
# Fail if a dependency update makes this version-specific workaround stale.
awk -v checksum="$checksum" '
    /^\[\[package\]\]/ { selected=0; version=0 }
    /^name = "surrealdb-diskann"$/ { selected=1 }
    selected && /^version = "0.61.0"$/ { version=1 }
    selected && version && $0 == "checksum = \"" checksum "\"" { found=1 }
    END { exit !found }
' "$ROOT/Cargo.lock" || {
    echo "DiskANN lockfile version/checksum changed; review $patch" >&2
    exit 1
}
mkdir -p "$base"
exec 9>"$base/$crate.lock"
flock 9
patch_hash=$(sha256sum "$patch" | cut -d ' ' -f 1)
if [[ -f "$dest/.patch-sha256" && $(<"$dest/.patch-sha256") == "$patch_hash" ]] &&
    git -C "$dest" apply --reverse --check "$patch"; then
    exit 0
fi
stage=$(mktemp -d "$base/.stage.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
archive="$stage/$crate.crate"
shopt -s nullglob
for cached in "${CARGO_HOME:-$HOME/.cargo}"/registry/cache/*/"$crate.crate"; do
    cp -- "$cached" "$archive"
    break
done
if [[ ! -f "$archive" ]]; then
    curl --fail --location --retry 3 --silent --show-error \
        "https://static.crates.io/crates/surrealdb-diskann/$crate.crate" -o "$archive"
fi
printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check --status
tar -xf "$archive" -C "$stage"
git -C "$stage/$crate" apply --check "$patch"
git -C "$stage/$crate" apply "$patch"
printf '%s\n' "$patch_hash" > "$stage/$crate/.patch-sha256"
rm -rf -- "$dest"
mv -- "$stage/$crate" "$dest"
