#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p "$ROOT/target"
scratch=$(mktemp -d "$ROOT/target/rental-test.XXXXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
testroot="$scratch/fixture"
work="$scratch/work"
mkdir -p "$testroot/deploy/rental" "$testroot/.rental/bin" "$work"
cp "$ROOT/deploy/rental/run.sh" "$testroot/deploy/rental/run.sh"
git -C "$testroot" init -q
git -C "$testroot" add deploy/rental/run.sh
git -C "$testroot" -c user.name=Test -c user.email=test@example.invalid commit -qm fixture
sha=$(git -C "$testroot" rev-parse HEAD)
printf '#!/usr/bin/env bash\nexit 0\n' > "$testroot/.rental/bin/all-db-benchctl"
cp "$testroot/.rental/bin/all-db-benchctl" "$testroot/.rental/bin/kvbench"
cp "$testroot/.rental/bin/all-db-benchctl" "$testroot/.rental/bin/recordbench"
cp "$testroot/.rental/bin/all-db-benchctl" "$testroot/.rental/bin/surrealdb-rocksdb-recordbench"
chmod +x "$testroot/.rental/bin/"*
binsha=$(sha256sum "$testroot/.rental/bin/kvbench" | awk '{print $1}')
jq -n --arg sha "$sha" --arg hash "$binsha" \
  '{format_version:1,compile_target:"x86-64-v2+aes",source_commit:$sha,binaries:{"all-db-benchctl":$hash,"kvbench":$hash,"recordbench":$hash,"surrealdb-rocksdb-recordbench":$hash}}' \
  > "$testroot/.rental/manifest.json"
runner="$testroot/deploy/rental/run.sh"
env RENTAL_WORK_ROOT="$work" "$runner" --preflight-only --profile smoke --lanes kv,record
jq '.compile_target="x86-64-v2"' "$testroot/.rental/manifest.json" > "$scratch/invalid-isa.json"
mv "$scratch/invalid-isa.json" "$testroot/.rental/manifest.json"
if env RENTAL_WORK_ROOT="$work" "$runner" --preflight-only --profile smoke --lanes kv >/dev/null 2>&1; then
  echo "unsupported ISA unexpectedly accepted" >&2; exit 1
fi
jq '.compile_target="x86-64-v2+aes"' "$testroot/.rental/manifest.json" > "$scratch/valid-isa.json"
mv "$scratch/valid-isa.json" "$testroot/.rental/manifest.json"
env RENTAL_WORK_ROOT="$work" "$runner" --dry-run --profile quick --lanes kv,record --run-id fixture-one > "$scratch/dry.log"
dry_text=$(cat "$scratch/dry.log")
[[ "$dry_text" == *"would run RUN_ID=fixture-one-kv-quick"* ]]
[[ "$dry_text" == *"would run RUN_ID=fixture-one-record-quick"* ]]
jq -e '.phase=="complete" and .source_commit != null' "$work/results/rental/fixture-one/status.json" >/dev/null
if env RENTAL_WORK_ROOT="$work" "$runner" --preflight-only --lanes bogus >/dev/null 2>&1; then
  echo "invalid lane unexpectedly accepted" >&2; exit 1
fi
if env RENTAL_WORK_ROOT="$work" "$runner" --preflight-only --lanes kv,kv >/dev/null 2>&1; then
  echo "duplicate lane unexpectedly accepted" >&2; exit 1
fi
# SHA mismatch must be caught before launching any work.
sed -i "s/$binsha/$(printf '0%.0s' {1..64})/g" "$testroot/.rental/manifest.json"
if env RENTAL_WORK_ROOT="$work" "$runner" --preflight-only --lanes kv >/dev/null 2>&1; then
  echo "corrupt binary identity unexpectedly accepted" >&2; exit 1
fi
if DBBENCH_MACHINE_ID_SHA256=oops "$ROOT/scripts/host-machine-id-sha256.sh" >/dev/null 2>&1; then
  echo "malformed machine ID unexpectedly accepted" >&2; exit 1
fi
test "$(DBBENCH_MACHINE_ID_SHA256=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA "$ROOT/scripts/host-machine-id-sha256.sh")" = "$(printf 'a%.0s' {1..64})"
# Exercise the orchestration and signal path without executing real databases.
mkdir -p "$testroot/scripts"
cat > "$testroot/scripts/run-kv-matrix.sh" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p "$root/results/runs/$RUN_ID"
printf '{"test":true}\n' > "$root/results/runs/$RUN_ID/results.ndjson"
sleep "${RENTAL_TEST_SLEEP:-0}"
FAKE
chmod +x "$testroot/scripts/run-kv-matrix.sh"
ln -s "$work/results" "$testroot/results"
# Restore original manifest hashes after corruption test.
jq --arg hash "$binsha" '.binaries |= with_entries(.value=$hash)' \
  "$testroot/.rental/manifest.json" > "$scratch/manifest.json"
mv "$scratch/manifest.json" "$testroot/.rental/manifest.json"
env RENTAL_WORK_ROOT="$work" RENTAL_RESULTS_PERSISTED=1 "$runner" \
  --profile smoke --lanes kv --run-id real-fixture > "$scratch/run.log"
jq -e '.phase=="complete" and .exit_code==0' \
  "$work/results/rental/real-fixture/status.json" >/dev/null
test -s "$work/results/runs/real-fixture-kv-smoke/results.ndjson"
env RENTAL_WORK_ROOT="$work" RENTAL_RESULTS_PERSISTED=1 RENTAL_TEST_SLEEP=20 "$runner" \
  --profile smoke --lanes kv --run-id cancelled-fixture > "$scratch/cancel.log" 2>&1 &
runner_pid=$!
sleep 1
kill -TERM "$runner_pid"
rc=0
wait "$runner_pid" || rc=$?
test "$rc" -eq 143
jq -e '.phase=="interrupted" and .exit_code==143' \
  "$work/results/rental/cancelled-fixture/status.json" >/dev/null
# Result upload must export only this run, not arbitrary data from a reused volume.
mkdir -p "$scratch/mockbin" "$work/results/runs/unrelated"
printf 'must-never-upload\n' > "$work/results/runs/unrelated/private.txt"
cat > "$scratch/mockbin/curl" <<'MOCK_CURL'
#!/usr/bin/env bash
set -euo pipefail
source_file=''
while (($#)); do
  case "$1" in
    --upload-file) source_file=$2; shift 2 ;;
    *) shift ;;
  esac
done
[[ -s "$source_file" ]]
cp -- "$source_file" "$RENTAL_CAPTURE_FILE"
MOCK_CURL
chmod +x "$scratch/mockbin/curl"
PATH="$scratch/mockbin:$PATH" RENTAL_CAPTURE_FILE="$scratch/upload.tar.zst" \
  RENTAL_WORK_ROOT="$work" RENTAL_RESULTS_PUT_URL=https://example.invalid/fixture \
  "$runner" --profile smoke --lanes kv --run-id upload-fixture > "$scratch/upload.log"
zstd -dc "$scratch/upload.tar.zst" | tar -tf - > "$scratch/upload.paths"
tar_paths=$(cat "$scratch/upload.paths")
[[ "$tar_paths" == *"results/rental/upload-fixture/status.json"* ]]
[[ "$tar_paths" == *"results/runs/upload-fixture-kv-smoke/results.ndjson"* ]]
[[ "$tar_paths" != *"unrelated"* ]]
echo "rental deployment contract tests passed"
