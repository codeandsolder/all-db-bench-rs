#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$ROOT/scripts/performance-runner-common.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/order"

performance_prepare_order "$tmp/order" fixed-initial "a" "b" "c"
[[ $(wc -l < "$tmp/order/jobs.txt") -eq 3 ]]
performance_prepare_order "$tmp/order" reshuffle-remaining "a" "b" "c"
set +e
performance_prepare_order "$tmp/order" fixed-initial "a" "b" "d"
order_rc=$?
set -e
[[ $order_rc -eq 2 ]]
! declare -F performance_scrub_case_pressure >/dev/null

# Plan-only initialization allocates storage without capacity probe or host capture.
mock_root="$tmp/mock-repo"
mkdir -p "$mock_root/scripts"
cat > "$mock_root/scripts/capture-host-metadata.sh" <<'HOST'
#!/usr/bin/env bash
printf 'captured-host\n' > "$1"
HOST
chmod +x "$mock_root/scripts/capture-host-metadata.sh"
MATRIX_PLAN_ONLY=1 DBBENCH_STORAGE_ROOT="$tmp/storage" \
  performance_prepare_matrix_run "$mock_root" plan 1000000 sustained
[[ "$RUN_DIR" == "$mock_root/results/runs/plan" ]]
[[ "$DATA_DIR" == "$tmp/storage/data/runs/plan" ]]
[[ ! -e "$RUN_DIR/host-start.txt" ]]

set +e
DBBENCH_STORAGE_ROOT="$tmp/storage" performance_prepare_matrix_run "$mock_root" low-space 1000000 reopen >/dev/null 2>&1
low_space_rc=$?
set -e
[[ $low_space_rc == 75 ]]
DBBENCH_STORAGE_ROOT="$tmp/storage" performance_prepare_matrix_run "$mock_root" captured 0 sustained
[[ $(cat "$RUN_DIR/host-start.txt") == captured-host ]]

# The fake Cargo command tests dispatch, not actual compilation.
cat > "$mock_root/scripts/cargo-local.sh" <<'CARGO'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$@" > "$CARGO_ARGS_TRACE"
mode=debug
bin=
while (($#)); do
  case "$1" in
    --release) mode=release; shift ;;
    --bin) bin=$2; shift 2 ;;
    *) shift ;;
  esac
done
[[ -n "$bin" ]]
mkdir -p "$CARGO_TARGET_DIR/$mode"
printf '#!/usr/bin/env bash\nexit 0\n' > "$CARGO_TARGET_DIR/$mode/$bin"
chmod +x "$CARGO_TARGET_DIR/$mode/$bin"
CARGO
chmod +x "$mock_root/scripts/cargo-local.sh"
CARGO_ARGS_TRACE="$tmp/cargo-args" performance_select_benchmark_binary "$mock_root" "$tmp/target" kv-all kvbench release
[[ "$BUILD_PROFILE" == release && "$BIN" == "$tmp/target/release/kvbench" && -x "$BIN" ]]
rg -Fxq -- '--release' "$tmp/cargo-args"
rg -Fxq -- '--locked' "$tmp/cargo-args"
rg -Fxq -- 'kv-all' "$tmp/cargo-args"
CARGO_ARGS_TRACE="$tmp/cargo-args" performance_select_benchmark_binary "$mock_root" "$tmp/target" record recordconcurrency debug
[[ "$BUILD_PROFILE" == debug && "$BIN" == "$tmp/target/debug/recordconcurrency" && -x "$BIN" ]]
! rg -Fxq -- '--release' "$tmp/cargo-args"
rg -Fxq -- '--locked' "$tmp/cargo-args"
BENCH_BIN=/bin/true performance_select_benchmark_binary "$mock_root" "$tmp/target" kv-all kvbench release
[[ "$BUILD_PROFILE" == external && "$BIN" == /bin/true ]]
(set +e; BENCH_BIN=/nonexistent/bin performance_select_benchmark_binary "$mock_root" "$tmp/target" kv-all kvbench release >/dev/null 2>&1; [[ $? == 2 ]])
(set +e; performance_select_benchmark_binary "$mock_root" "$tmp/target" kv-all kvbench invalid >/dev/null 2>&1; [[ $? == 2 ]])

BIN=/bin/true
BUILD_PROFILE=external
BENCH_SOURCE_COMMIT=$(git -C "$ROOT" rev-parse HEAD)
performance_capture_benchmark_source "$ROOT"
[[ "$BIN_SHA" == "$(sha256sum /bin/true | awk '{print $1}')" ]]
[[ "$HARNESS_COMMIT" == "$BENCH_SOURCE_COMMIT" ]]
(set +e; BENCH_SOURCE_COMMIT=invalid performance_capture_benchmark_source "$ROOT" >/dev/null 2>&1; [[ $? == 2 ]])
performance_capture_benchmark_context "$ROOT" "$ROOT/scripts/run-reopen-matrix.sh" "$tmp"
[[ "$COMMON_SHA" == "$(sha256sum "$ROOT/scripts/performance-runner-common.sh" | awk '{print $1}')" ]]
[[ "$CONTINUOUS_NOISE_SHA" == "$NOISE_SHA" ]]
[[ "$RUNNER_SHA" == "$(sha256sum "$ROOT/scripts/run-reopen-matrix.sh" | awk '{print $1}')" ]]

echo performance-runner-common-ok
