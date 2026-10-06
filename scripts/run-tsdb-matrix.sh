#!/usr/bin/env bash
set -euo pipefail
PROFILE=${1:-smoke}
ROOT=${ROOT:-$(cd -- "$(dirname -- "$0")/.." && pwd)}
cd "$ROOT"

case "$PROFILE" in
  smoke)
    TRIALS=1; SERIES=32; SAMPLES=20; BATCH_SERIES=16; QUERY_ITERATIONS=3; MIN_FREE_GIB=2
    ;;
  quick)
    TRIALS=3; SERIES=1000; SAMPLES=120; BATCH_SERIES=50; QUERY_ITERATIONS=20; MIN_FREE_GIB=10
    ;;
  full)
    TRIALS=5; SERIES=10000; SAMPLES=360; BATCH_SERIES=100; QUERY_ITERATIONS=100; MIN_FREE_GIB=30
    ;;
  *) echo "usage: $0 [smoke|quick|full]" >&2; exit 2 ;;
esac
TRIALS=${TSDB_TRIALS_OVERRIDE:-$TRIALS}
SERIES=${TSDB_SERIES_OVERRIDE:-$SERIES}
SAMPLES=${TSDB_SAMPLES_OVERRIDE:-$SAMPLES}
BATCH_SERIES=${TSDB_BATCH_SERIES_OVERRIDE:-$BATCH_SERIES}
QUERY_ITERATIONS=${TSDB_QUERY_ITERATIONS_OVERRIDE:-$QUERY_ITERATIONS}

for tool in curl sha256sum uv awk shuf du cmp sort find findmnt hostname; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing required tool: $tool" >&2; exit 2; }
done

"$ROOT/scripts/ensure-tsdb-binaries.sh" >/dev/null
DEPS=${TSDB_DEPS_DIR:-"$ROOT/.deps/tsdb"}
GREPTIME="$DEPS/greptimedb-1.2.1/greptime"
VICTORIA="$DEPS/victoriametrics-1.153.0/victoria-metrics-prod"
PROMETHEUS="$DEPS/prometheus-3.15.0/prometheus"
INFLUXDB3="$DEPS/influxdb3-3.12.0/influxdb3"
for binary in "$GREPTIME" "$VICTORIA" "$PROMETHEUS" "$INFLUXDB3"; do
  [[ -x "$binary" ]] || { echo "missing TSDB binary after install: $binary" >&2; exit 2; }
done

TARGET_DIR=${TSDB_CARGO_TARGET_DIR:-/tmp/rust-db-tsdb-target}
TSDB_MANIFEST="$ROOT/engines/tsdb-server/Cargo.toml"
if [[ "$PROFILE" == smoke ]]; then
  BIN="$TARGET_DIR/debug/tsdb-server-bench"
  "$ROOT/scripts/cargo-local-1.99.sh" build --locked --manifest-path "$TSDB_MANIFEST" --target-dir "$TARGET_DIR"
else
  BIN="$TARGET_DIR/release/tsdb-server-bench"
  "$ROOT/scripts/cargo-local-1.99.sh" build --release --locked --manifest-path "$TSDB_MANIFEST" --target-dir "$TARGET_DIR"
fi
BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}')

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-tsdb-$PROFILE"}
if [[ "${TSDB_RESUME_SHUFFLE_REMAINING:-0}" == 1 ]]; then
  RESUME_ORDER_POLICY=reshuffle-remaining
else
  RESUME_ORDER_POLICY=fixed-initial
fi
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,client,server-before,server-after,stderr,server-logs,server-configs,noise} "$DATA_DIR"
HOST_NAME=$(hostname)
MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
FILESYSTEM=$(findmnt -T "$DATA_DIR" -n -o FSTYPE 2>/dev/null || true)
SOURCE=$(findmnt -T "$DATA_DIR" -n -o SOURCE 2>/dev/null || true)
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
GREPTIME_SHA=$(sha256sum "$GREPTIME" | awk '{print $1}')
VICTORIA_SHA=$(sha256sum "$VICTORIA" | awk '{print $1}')
PROMETHEUS_SHA=$(sha256sum "$PROMETHEUS" | awk '{print $1}')
INFLUXDB3_SHA=$(sha256sum "$INFLUXDB3" | awk '{print $1}')
INFLUXDB3_PYTHON_LIB=$(find "$DEPS/influxdb3-3.12.0/python/lib" -maxdepth 1 -type f -name 'libpython*.so.1.0' -print -quit)
[[ -n "$INFLUXDB3_PYTHON_LIB" ]] || { echo "missing bundled InfluxDB libpython runtime" >&2; exit 2; }
INFLUXDB3_PYTHON_SHA=$(sha256sum "$INFLUXDB3_PYTHON_LIB" | awk '{print $1}')
RUNNER_SHA=$(sha256sum "$ROOT/scripts/run-tsdb-matrix.sh" | awk '{print $1}')
INSTALLER_SHA=$(sha256sum "$ROOT/scripts/ensure-tsdb-binaries.sh" | awk '{print $1}')
PROCESS_CAPTURE_SHA=$(sha256sum "$ROOT/scripts/capture-process-tree.py" | awk '{print $1}')
MERGE_RESULT_SHA=$(sha256sum "$ROOT/scripts/merge-tsdb-result.py" | awk '{print $1}')
SUMMARIZER_SHA=$(sha256sum "$ROOT/scripts/summarize-tsdb.py" | awk '{print $1}')
NOISE_GUARD_SHA=$(sha256sum "$ROOT/scripts/check-external-noise.py" | awk '{print $1}')
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
if (( free_bytes < min_free_bytes )); then
  echo "refusing TSDB run: free=$free_bytes required=$min_free_bytes" >&2
  exit 75
fi

check_quiet_host() {
  local phase=${1:-case} io_psi10
  [[ "$PROFILE" == smoke || "${ALLOW_BUSY:-0}" == 1 ]] && return 0
  io_psi10=$(awk '/^full / {for(i=1;i<=NF;i++) if($i ~ /^avg10=/){split($i,a,"="); print a[2]}}' /proc/pressure/io)
  if ! awk -v p="${io_psi10:-0}" 'BEGIN { exit !(p <= 5.0) }'; then
    echo "refusing TSDB performance case under I/O pressure ($phase): io PSI full avg10=${io_psi10}%" >&2
    return 75
  fi
}

check_external_noise() {
  local phase=$1 server_pid=${2:-} evidence=$3 rc
  [[ "$PROFILE" == smoke || "${ALLOW_EXTERNAL_NOISE:-0}" == 1 ]] && return 0
  local args=(--json-out "$evidence")
  [[ -n "$server_pid" ]] && args+=(--exclude-pid "$server_pid")
  set +e
  uv run --script "$ROOT/scripts/check-external-noise.py" "${args[@]}"
  rc=$?
  set -e
  if (( rc != 0 )); then
    echo "refusing TSDB performance case due to external host work ($phase): $(cat "$evidence" 2>/dev/null)" >&2
  fi
  return "$rc"
}

wait_http() {
  local url=$1 pid=$2 deadline=$((SECONDS + 45))
  while (( SECONDS < deadline )); do
    kill -0 "$pid" 2>/dev/null || return 1
    if curl -fsS --max-time 1 "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

SERVER_PID=""
stop_server() {
  local pid=${SERVER_PID:-}
  [[ -n "$pid" ]] || return 0
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || true
    local deadline=$((SECONDS + 15))
    while kill -0 "$pid" 2>/dev/null && (( SECONDS < deadline )); do sleep 0.1; done
    if kill -0 "$pid" 2>/dev/null; then kill -KILL "$pid" 2>/dev/null || true; fi
  fi
  wait "$pid" 2>/dev/null || true
  SERVER_PID=""
}
trap stop_server EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

start_server() {
  local engine=$1 case_data=$2 log=$3
  local start_ns end_ns cfg
  start_ns=$(date +%s%N)
  case "$engine" in
    greptimedb)
      ENDPOINT=http://127.0.0.1:19000
      VERSION=1.2.1
        "$GREPTIME" standalone start \
        --data-home "$case_data" \
        --http-addr 127.0.0.1:19000 --grpc-bind-addr 127.0.0.1:19001 \
        --mysql-addr 127.0.0.1:19002 --postgres-addr 127.0.0.1:19003 \
        >"$log" 2>&1 &
      SERVER_PID=$!
      HEALTH_URL="$ENDPOINT/health"
      ;;
    victoriametrics)
      ENDPOINT=http://127.0.0.1:19010
      VERSION=1.153.0
      "$VICTORIA" -storageDataPath="$case_data" -httpListenAddr=127.0.0.1:19010 \
        -retentionPeriod=100y >"$log" 2>&1 &
      SERVER_PID=$!
      HEALTH_URL="$ENDPOINT/health"
      ;;
    prometheus)
      ENDPOINT=http://127.0.0.1:19020
      VERSION=3.15.0
      cfg="$RUN_DIR/server-configs/$case_id.prometheus.yml"
      mkdir -p "$case_data"
      cat > "$cfg" <<'YAML'
global:
  scrape_interval: 1h
scrape_configs: []
YAML
      "$PROMETHEUS" --config.file="$cfg" --storage.tsdb.path="$case_data/tsdb" \
        --web.listen-address=127.0.0.1:19020 --web.enable-remote-write-receiver \
        --storage.tsdb.retention.time=365d --log.level=error >"$log" 2>&1 &
      SERVER_PID=$!
      HEALTH_URL="$ENDPOINT/-/ready"
      ;;
    influxdb3)
      ENDPOINT=http://127.0.0.1:19030
      VERSION=3.12.0
      "$INFLUXDB3" serve --node-id bench-node --object-store file --data-dir "$case_data" \
        --http-bind 127.0.0.1:19030 --without-auth >"$log" 2>&1 &
      SERVER_PID=$!
      HEALTH_URL="$ENDPOINT/health"
      ;;
    *) echo "unknown TSDB engine: $engine" >&2; return 2 ;;
  esac
  if ! wait_http "$HEALTH_URL" "$SERVER_PID"; then
    echo "TSDB server $engine did not become ready; log follows" >&2
    tail -100 "$log" >&2 || true
    return 1
  fi
  end_ns=$(date +%s%N)
  STARTUP_S=$(awk -v a="$start_ns" -v b="$end_ns" 'BEGIN { printf "%.9f", (b-a)/1000000000.0 }')
}

ENGINES=(greptimedb victoriametrics prometheus influxdb3)
if [[ -n "${ENGINES_OVERRIDE:-}" ]]; then read -r -a ENGINES <<< "$ENGINES_OVERRIDE"; fi
SELECTED_ENGINES="${ENGINES[*]}"

SUPPORT_NEW="$RUN_DIR/support.json.new"
cat > "$SUPPORT_NEW" <<EOF_SUPPORT
{
  "lane": "tsdb-server",
  "profile": "$PROFILE",
  "benchmark_binary_sha256": "$BIN_SHA",
  "harness_sha256": {
    "runner": "$RUNNER_SHA",
    "installer": "$INSTALLER_SHA",
    "process_capture": "$PROCESS_CAPTURE_SHA",
    "result_merge": "$MERGE_RESULT_SHA",
    "summarizer": "$SUMMARIZER_SHA",
    "noise_guard": "$NOISE_GUARD_SHA"
  },
  "hostname": "$HOST_NAME",
  "machine_id_sha256": "$MACHINE_ID_SHA256",
  "filesystem": "$FILESYSTEM",
  "source": "$SOURCE",
  "server_binary_sha256": {
    "greptimedb": "$GREPTIME_SHA",
    "victoriametrics": "$VICTORIA_SHA",
    "prometheus": "$PROMETHEUS_SHA",
    "influxdb3": "$INFLUXDB3_SHA"
  },
  "server_runtime_sha256": {
    "influxdb3_libpython": "$INFLUXDB3_PYTHON_SHA"
  },
  "trials": $TRIALS,
  "resume_order_policy": "$RESUME_ORDER_POLICY",
  "selected_engines": "$SELECTED_ENGINES",
  "series": $SERIES,
  "samples_per_series": $SAMPLES,
  "batch_series": $BATCH_SERIES,
  "query_iterations": $QUERY_ITERATIONS,
  "step_ms": 10000,
  "engines": {
    "greptimedb": "1.2.1",
    "victoriametrics": "1.153.0",
    "prometheus": "3.15.0",
    "influxdb3": "3.12.0"
  },
  "dataset": "bench_metric{host=hNNNNNN,region=rNN} with one float value every 10 seconds",
  "transport_policy": "Prometheus Remote Write v1 for GreptimeDB/VictoriaMetrics/Prometheus; InfluxDB v3 line protocol for InfluxDB 3 Core; protocol is explicit in every result",
  "query_policy": "equivalent latest-series, full-range single-series, and latest-timestamp aggregate semantics; PromQL for Prometheus-shaped engines and SQL for InfluxDB 3",
  "durability_note": "server-native acknowledgement semantics are retained and must not be interpreted as byte-identical durability contracts",
  "storage_footprint_point": "after graceful server shutdown; CPU/runqueue/process-I/O accounting remains bracketed around the client interval"
}
EOF_SUPPORT

EXISTING_CASES=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
if [[ -s "$RUN_DIR/support.json" ]]; then
  if ! cmp -s "$RUN_DIR/support.json" "$SUPPORT_NEW"; then
    if (( EXISTING_CASES > 0 )); then
      rm -f "$SUPPORT_NEW"
      echo "refusing TSDB resume: existing support.json disagrees with current host/binary/profile/dataset identity" >&2
      exit 2
    fi
    mv "$SUPPORT_NEW" "$RUN_DIR/support.json"
  else
    rm -f "$SUPPORT_NEW"
  fi
else
  mv "$SUPPORT_NEW" "$RUN_DIR/support.json"
fi

JOBS=()
for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do JOBS+=("$trial|$engine"); done
done
if [[ -s "$RUN_DIR/jobs.txt" ]]; then
  expected_jobs=$(mktemp)
  existing_jobs=$(mktemp)
  printf '%s\n' "${JOBS[@]}" | sort > "$expected_jobs"
  sort "$RUN_DIR/jobs.txt" > "$existing_jobs"
  if ! cmp -s "$expected_jobs" "$existing_jobs"; then
    rm -f "$expected_jobs" "$existing_jobs"
    echo "refusing TSDB resume: jobs.txt does not match the configured trial/engine set" >&2
    exit 2
  fi
  rm -f "$expected_jobs" "$existing_jobs"
  mapfile -t ORDERED < "$RUN_DIR/jobs.txt"
else
  mapfile -t ORDERED < <(printf '%s\n' "${JOBS[@]}" | shuf)
  printf '%s\n' "${ORDERED[@]}" > "$RUN_DIR/jobs.txt"
fi
if [[ "$RESUME_ORDER_POLICY" == reshuffle-remaining ]]; then
  mapfile -t ORDERED < <(printf '%s\n' "${ORDERED[@]}" | shuf)
fi

clear_failure() {
  local case_id=$1 file="$RUN_DIR/failures.ndjson" tmp
  [[ -f "$file" ]] || return 0
  tmp="${file}.tmp"
  grep -v -F "\"case_id\":\"$case_id\"" "$file" > "$tmp" || true
  mv "$tmp" "$file"
  [[ -s "$file" ]] || rm -f "$file"
}

INDEX=0
TOTAL=${#ORDERED[@]}
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1))
  IFS='|' read -r trial engine <<< "$job"
  case_id="t${trial}-${engine}"
  out="$RUN_DIR/cases/$case_id.json"
  client_out="$RUN_DIR/client/$case_id.json"
  before_out="$RUN_DIR/server-before/$case_id.json"
  after_out="$RUN_DIR/server-after/$case_id.json"
  log="$RUN_DIR/server-logs/$case_id.log"
  err="$RUN_DIR/stderr/$case_id.log"
  case_data="$DATA_DIR/$case_id"
  noise_before="$RUN_DIR/noise/$case_id.before.json"
  noise_after="$RUN_DIR/noise/$case_id.after.json"

  if [[ -s "$out" ]] && uv run --no-project python -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["lane"]=="tsdb-server" and d["trial"]==int(sys.argv[2]) and d["engine"]==sys.argv[3]' "$out" "$trial" "$engine" >/dev/null 2>&1; then
    clear_failure "$case_id"
    continue
  fi
  rm -rf -- "$case_data"
  rm -f "$out" "$client_out" "$before_out" "$after_out" "$err" "$log" "$noise_before" "$noise_after"
  clear_failure "$case_id"
  mkdir -p "$case_data"
  echo "[$INDEX/$TOTAL] $case_id" >&2

  check_quiet_host "before-server:$case_id" || exit $?
  check_external_noise "before-server:$case_id" "" "$noise_before" || exit $?
  if ! start_server "$engine" "$case_data" "$log"; then
    printf '{"case_id":"%s","stage":"server-start","returncode":1}\n' "$case_id" >> "$RUN_DIR/failures.ndjson"
    stop_server
    continue
  fi
  sleep 1
  check_external_noise "before-client:$case_id" "$SERVER_PID" "$noise_before" || { rc=$?; stop_server; exit "$rc"; }
  uv run --script "$ROOT/scripts/capture-process-tree.py" --pid "$SERVER_PID" --output "$before_out"

  set +e
  "$BIN" --engine "$engine" --engine-version "$VERSION" --endpoint "$ENDPOINT" \
    --series "$SERIES" --samples-per-series "$SAMPLES" --step-ms 10000 \
    --batch-series "$BATCH_SERIES" --query-iterations "$QUERY_ITERATIONS" \
    --trial "$trial" --scenario "tsdb-${PROFILE}" --output "$client_out" 2>"$err"
  rc=$?
  set -e
  if (( rc == 0 )); then
    uv run --script "$ROOT/scripts/capture-process-tree.py" --pid "$SERVER_PID" --output "$after_out" || rc=$?
  fi
  noise_rc=0
  if (( rc == 0 )); then
    check_external_noise "after-client:$case_id" "$SERVER_PID" "$noise_after" || noise_rc=$?
    if [[ ${noise_rc:-0} -ne 0 ]]; then
      stop_server
      rm -f "$client_out" "$before_out" "$after_out"
      exit "$noise_rc"
    fi
  fi

  # Resource accounting ends before shutdown; storage footprint is sampled only
  # after a graceful stop so buffered/compacted state is represented on disk.
  stop_server
  if (( rc == 0 )); then
    set +e
    uv run --script "$ROOT/scripts/merge-tsdb-result.py" \
      --client "$client_out" --server-before "$before_out" --server-after "$after_out" \
      --data-dir "$case_data" --startup-s "$STARTUP_S" --server-log "$log" --output "$out"
    rc=$?
    set -e
  fi
  if (( rc == 0 )); then
    clear_failure "$case_id"
  else
    rm -f "$out"
    printf '{"case_id":"%s","stage":"client-or-merge","returncode":%d,"stderr":"%s"}\n' "$case_id" "$rc" "$err" >> "$RUN_DIR/failures.ndjson"
  fi
  [[ "${KEEP_TSDB_DATA:-0}" == 1 ]] || rm -rf -- "$case_data"
  [[ -s "$err" ]] || rm -f "$err"
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
COMPLETED=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
FAILURES=0
[[ -s "$RUN_DIR/failures.ndjson" ]] && FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson")
summary_rc=0
uv run --script "$ROOT/scripts/summarize-tsdb.py" "$RUN_DIR" --expect-trials "$TRIALS" \
  --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md" || summary_rc=$?
echo "run=$RUN_ID total=$TOTAL completed=$COMPLETED failures=$FAILURES summary_rc=$summary_rc results=$RUN_DIR/results.ndjson"
(( FAILURES == 0 && COMPLETED == TOTAL && summary_rc == 0 ))
