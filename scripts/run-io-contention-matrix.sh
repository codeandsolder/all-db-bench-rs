#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
for tool in fio jq findmnt sha256sum uv awk realpath shuf; do
  command -v "$tool" >/dev/null 2>&1 || { echo "required tool not found: $tool" >&2; exit 127; }
done
BASELINE_DIR=${2:-${BASELINE_DIR:-}}
if [[ -z "$BASELINE_DIR" ]]; then
  echo "usage: $0 [smoke|quick|full] BASELINE_RUN_DIR" >&2
  echo "BASELINE_RUN_DIR must contain support.json and the pressure-calibration fio result from run-io-baseline.sh" >&2
  exit 2
fi

BASELINE_DIR=$(realpath "$BASELINE_DIR") || { echo "invalid baseline directory: $BASELINE_DIR" >&2; exit 2; }
BASELINE_SUPPORT="$BASELINE_DIR/support.json"
BASELINE_MANIFEST="$BASELINE_DIR/calibration.json"
BASELINE_SUMMARY="$BASELINE_DIR/summary.json"
[[ -s "$BASELINE_MANIFEST" ]] || { echo "missing completed calibration manifest: $BASELINE_MANIFEST" >&2; exit 2; }
[[ -s "$BASELINE_SUMMARY" ]] || { echo "missing completed calibration summary: $BASELINE_SUMMARY" >&2; exit 2; }
if ! jq -e '.format_version == 1 and .lane == "io-calibration" and .complete == true and .calibration_protocol_version == 4' "$BASELINE_MANIFEST" >/dev/null; then
  echo "invalid or incomplete calibration manifest: $BASELINE_MANIFEST" >&2
  exit 2
fi
[[ -s "$BASELINE_SUPPORT" ]] || {
  echo "missing $BASELINE_SUPPORT; rerun run-io-baseline.sh with the current calibration protocol" >&2
  exit 2
}
CALIBRATION_PROTOCOL_VERSION=$(jq -r '.calibration_protocol_version // 0' "$BASELINE_SUPPORT")
BASELINE_PROFILE=$(jq -r '.profile // empty' "$BASELINE_SUPPORT")
READ_CALIBRATION_FILE=$(jq -r '.pressure_calibration.read.file // empty' "$BASELINE_SUPPORT")
WRITE_CALIBRATION_FILE=$(jq -r '.pressure_calibration.write.file // empty' "$BASELINE_SUPPORT")
PRESSURE_BS_BYTES=$(jq -r '.pressure_calibration.bs_bytes // 0' "$BASELINE_SUPPORT")
READ_FRACTION=$(jq -r '.pressure_calibration.read_fraction // 0' "$BASELINE_SUPPORT")
WRITE_FRACTION=$(jq -r '.pressure_calibration.write_fraction // 0' "$BASELINE_SUPPORT")
BASELINE_HOST_NAME=$(jq -r '.host_name // empty' "$BASELINE_SUPPORT")
BASELINE_MACHINE_ID_SHA256=$(jq -r '.machine_id_sha256 // empty' "$BASELINE_SUPPORT")
BASELINE_FSTYPE=$(jq -r '.filesystem // empty' "$BASELINE_SUPPORT")
BASELINE_SOURCE=$(jq -r '.source // empty' "$BASELINE_SUPPORT")
[[ "$CALIBRATION_PROTOCOL_VERSION" == 4 ]] || {
  echo "unsupported baseline calibration protocol: $CALIBRATION_PROTOCOL_VERSION (need 4)" >&2
  exit 2
}
[[ -n "$READ_CALIBRATION_FILE" && -n "$WRITE_CALIBRATION_FILE" ]] || {
  echo "baseline support.json has no protocol-v4 directional calibration files" >&2
  exit 2
}
[[ "$PRESSURE_BS_BYTES" =~ ^[0-9]+$ ]] && (( PRESSURE_BS_BYTES >= 4096 )) || {
  echo "invalid pressure calibration block size: $PRESSURE_BS_BYTES" >&2
  exit 2
}
if ! awk -v r="$READ_FRACTION" -v w="$WRITE_FRACTION" 'BEGIN { exit !(r > 0 && w > 0 && (r+w) > 0.999999 && (r+w) < 1.000001) }'; then
  echo "invalid pressure mix fractions: read=$READ_FRACTION write=$WRITE_FRACTION" >&2
  exit 2
fi
READ_BASELINE_JSON="$BASELINE_DIR/$READ_CALIBRATION_FILE"
WRITE_BASELINE_JSON="$BASELINE_DIR/$WRITE_CALIBRATION_FILE"
[[ -s "$READ_BASELINE_JSON" && -s "$WRITE_BASELINE_JSON" ]] || {
  echo "missing one or both calibrated directional pressure baselines" >&2
  exit 2
}
READ_BASELINE_SHA=$(sha256sum "$READ_BASELINE_JSON" | awk '{print $1}')
WRITE_BASELINE_SHA=$(sha256sum "$WRITE_BASELINE_JSON" | awk '{print $1}')
BASELINE_SUPPORT_SHA=$(sha256sum "$BASELINE_SUPPORT" | awk '{print $1}')
BASELINE_SUMMARY_SHA=$(sha256sum "$BASELINE_SUMMARY" | awk '{print $1}')
MANIFEST_READ_SHA=$(jq -r '.pressure_calibration.read.sha256 // empty' "$BASELINE_MANIFEST")
MANIFEST_WRITE_SHA=$(jq -r '.pressure_calibration.write.sha256 // empty' "$BASELINE_MANIFEST")
MANIFEST_SUPPORT_SHA=$(jq -r '.support_sha256 // empty' "$BASELINE_MANIFEST")
MANIFEST_SUMMARY_SHA=$(jq -r '.summary_sha256 // empty' "$BASELINE_MANIFEST")
if [[ "$READ_BASELINE_SHA" != "$MANIFEST_READ_SHA" || "$WRITE_BASELINE_SHA" != "$MANIFEST_WRITE_SHA" || "$BASELINE_SUPPORT_SHA" != "$MANIFEST_SUPPORT_SHA" || "$BASELINE_SUMMARY_SHA" != "$MANIFEST_SUMMARY_SHA" ]]; then
  echo "calibration manifest hash mismatch: directional baseline/support/summary data changed after calibration completed" >&2
  exit 2
fi
MANIFEST_PRESSURE_BS=$(jq -r '.pressure_calibration.bs_bytes // 0' "$BASELINE_MANIFEST")
if [[ "$MANIFEST_PRESSURE_BS" != "$PRESSURE_BS_BYTES" ]]; then
  echo "calibration manifest block-size mismatch: manifest=$MANIFEST_PRESSURE_BS support=$PRESSURE_BS_BYTES" >&2
  exit 2
fi
if ! jq -e --arg bs "$PRESSURE_BS_BYTES" '
  (.jobs | length) == 1 and (.jobs[0].error // 0) == 0 and
  .jobs[0]["job options"].rw == "randread" and .jobs[0]["job options"].bs == $bs and
  .jobs[0]["job options"].ba == $bs and .jobs[0]["job options"].ioengine == "psync" and
  .jobs[0]["job options"].direct == "1"
' "$READ_BASELINE_JSON" >/dev/null; then
  echo "read baseline fio JSON does not match protocol-v4 calibration semantics" >&2
  exit 2
fi
if ! jq -e --arg bs "$PRESSURE_BS_BYTES" '
  (.jobs | length) == 1 and (.jobs[0].error // 0) == 0 and
  .jobs[0]["job options"].rw == "randwrite" and .jobs[0]["job options"].bs == $bs and
  .jobs[0]["job options"].ba == $bs and .jobs[0]["job options"].ioengine == "psync" and
  .jobs[0]["job options"].direct == "1"
' "$WRITE_BASELINE_JSON" >/dev/null; then
  echo "write baseline fio JSON does not match protocol-v4 calibration semantics" >&2
  exit 2
fi

case "$PROFILE" in
  smoke)
    TRIALS=1; RECORDS=5000; OPS=2000; PRESSURE_SIZE=256M; MIN_FREE_GIB=2; LEVELS=(0 30 60)
    ;;
  quick)
    TRIALS=3; RECORDS=100000; OPS=50000; PRESSURE_SIZE=2G; MIN_FREE_GIB=10; LEVELS=(0 10 30 60)
    [[ "$BASELINE_PROFILE" == quick || "$BASELINE_PROFILE" == full ]] || {
      echo "quick contention requires a quick/full storage baseline, got profile=$BASELINE_PROFILE" >&2
      exit 2
    }
    ;;
  full)
    TRIALS=7; RECORDS=1000000; OPS=250000; PRESSURE_SIZE=8G; MIN_FREE_GIB=30; LEVELS=(0 10 30 60 90)
    [[ "$BASELINE_PROFILE" == full ]] || {
      echo "full contention requires a full storage baseline, got profile=$BASELINE_PROFILE" >&2
      exit 2
    }
    ;;
  *) echo "usage: $0 [smoke|quick|full] BASELINE_RUN_DIR" >&2; exit 2 ;;
esac

ROOT=${ROOT:-$(cd -- "$(dirname -- "$0")/.." && pwd)}
CURRENT_HOST_NAME=$(hostname)
CURRENT_MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
if [[ -z "$BASELINE_HOST_NAME" || -z "$BASELINE_MACHINE_ID_SHA256" || -z "$BASELINE_FSTYPE" || -z "$BASELINE_SOURCE" ]]; then
  echo "baseline support.json lacks protocol-v4 host/filesystem provenance" >&2
  exit 2
fi
if [[ "$CURRENT_MACHINE_ID_SHA256" != "$BASELINE_MACHINE_ID_SHA256" || "$CURRENT_HOST_NAME" != "$BASELINE_HOST_NAME" ]]; then
  echo "baseline host mismatch: baseline=$BASELINE_HOST_NAME current=$CURRENT_HOST_NAME" >&2
  exit 2
fi
MANIFEST_HOST=$(jq -r '.hostname // empty' "$BASELINE_MANIFEST")
MANIFEST_MACHINE=$(jq -r '.machine_id_sha256 // empty' "$BASELINE_MANIFEST")
MANIFEST_FSTYPE=$(jq -r '.storage.filesystem // empty' "$BASELINE_MANIFEST")
MANIFEST_SOURCE=$(jq -r '.storage.source // empty' "$BASELINE_MANIFEST")
if [[ "$MANIFEST_HOST" != "$BASELINE_HOST_NAME" || "$MANIFEST_MACHINE" != "$BASELINE_MACHINE_ID_SHA256" || "$MANIFEST_FSTYPE" != "$BASELINE_FSTYPE" || "$MANIFEST_SOURCE" != "$BASELINE_SOURCE" ]]; then
  echo "calibration manifest provenance disagrees with support.json" >&2
  exit 2
fi
CPUS=$(getconf _NPROCESSORS_ONLN)
check_quiet_host() {
  local phase=${1:-start} load1 io_psi10
  load1=$(awk '{print $1}' /proc/loadavg)
  io_psi10=$(awk '/^full / {for(i=1;i<=NF;i++) if($i ~ /^avg10=/){split($i,a,"="); print a[2]}}' /proc/pressure/io)
  if ! awk -v l="$load1" -v c="$CPUS" 'BEGIN { exit !(l <= c * 0.5) }'; then
    echo "refusing I/O-contention benchmark on busy host ($phase): load1=$load1 visible_cpus=$CPUS" >&2
    return 75
  fi
  if ! awk -v p="${io_psi10:-0}" 'BEGIN { exit !(p <= 5.0) }'; then
    echo "refusing I/O-contention benchmark under existing I/O pressure ($phase): io PSI full avg10=${io_psi10}%" >&2
    return 75
  fi
}
if [[ "${ALLOW_BUSY:-0}" != 1 ]]; then
  check_quiet_host start || exit $?
fi

BASE_READ_IOPS=$(jq -r '.jobs[0].read.iops // 0' "$READ_BASELINE_JSON")
BASE_WRITE_IOPS=$(jq -r '.jobs[0].write.iops // 0' "$WRITE_BASELINE_JSON")
BASE_IOPS=$(awk -v r="$BASE_READ_IOPS" -v w="$BASE_WRITE_IOPS" -v rf="$READ_FRACTION" -v wf="$WRITE_FRACTION" 'BEGIN { a=r/rf; b=w/wf; printf "%.12g", (a < b ? a : b) }')
if ! awk -v r="$BASE_READ_IOPS" -v w="$BASE_WRITE_IOPS" -v c="$BASE_IOPS" 'BEGIN { exit !(r > 0 && w > 0 && c > 0) }'; then
  echo "invalid directional/composite baseline IOPS: read=$BASE_READ_IOPS write=$BASE_WRITE_IOPS composite=$BASE_IOPS" >&2
  exit 2
fi
MANIFEST_READ_IOPS=$(jq -r '.pressure_calibration.read.iops // 0' "$BASELINE_MANIFEST")
MANIFEST_WRITE_IOPS=$(jq -r '.pressure_calibration.write.iops // 0' "$BASELINE_MANIFEST")
MANIFEST_BASE_IOPS=$(jq -r '.pressure_calibration.iops // 0' "$BASELINE_MANIFEST")
if ! awk -v ar="$BASE_READ_IOPS" -v br="$MANIFEST_READ_IOPS" -v aw="$BASE_WRITE_IOPS" -v bw="$MANIFEST_WRITE_IOPS" -v ac="$BASE_IOPS" -v bc="$MANIFEST_BASE_IOPS" 'BEGIN { dr=ar-br; if(dr<0)dr=-dr; dw=aw-bw; if(dw<0)dw=-dw; dc=ac-bc; if(dc<0)dc=-dc; exit !(br>0 && bw>0 && bc>0 && dr/(ar>1?ar:1)<1e-9 && dw/(aw>1?aw:1)<1e-9 && dc/(ac>1?ac:1)<1e-9) }'; then
  echo "calibration manifest IOPS mismatch: raw read/write/composite=$BASE_READ_IOPS/$BASE_WRITE_IOPS/$BASE_IOPS manifest=$MANIFEST_READ_IOPS/$MANIFEST_WRITE_IOPS/$MANIFEST_BASE_IOPS" >&2
  exit 2
fi
PREP_BS_BYTES=1048576
if (( PRESSURE_BS_BYTES > PREP_BS_BYTES )); then PREP_BS_BYTES=$PRESSURE_BS_BYTES; fi

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-io-contention-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
mkdir -p "$RUN_DIR"/{cases,stderr,pressure,storage} "$DATA_DIR"
CURRENT_FSTYPE=$(findmnt -T "$DATA_DIR" -n -o FSTYPE 2>/dev/null || true)
CURRENT_SOURCE=$(findmnt -T "$DATA_DIR" -n -o SOURCE 2>/dev/null || true)
if [[ "$CURRENT_FSTYPE" != "$BASELINE_FSTYPE" || "$CURRENT_SOURCE" != "$BASELINE_SOURCE" ]]; then
  echo "baseline filesystem mismatch: baseline=$BASELINE_FSTYPE:$BASELINE_SOURCE current=$CURRENT_FSTYPE:$CURRENT_SOURCE" >&2
  exit 2
fi
free_bytes=$(df -B1 --output=avail "$DATA_DIR" | tail -n1 | tr -d ' ')
min_free_bytes=$((MIN_FREE_GIB * 1024 * 1024 * 1024))
if (( free_bytes < min_free_bytes )); then
  echo "refusing I/O-contention run: free=$free_bytes required=$min_free_bytes" >&2
  exit 75
fi

"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT" || exit $?
cp "$READ_BASELINE_JSON" "$RUN_DIR/baseline-pressure-read.json" || exit $?
cp "$WRITE_BASELINE_JSON" "$RUN_DIR/baseline-pressure-write.json" || exit $?
cp "$BASELINE_SUPPORT" "$RUN_DIR/baseline-support.json" || exit $?
cp "$BASELINE_MANIFEST" "$RUN_DIR/baseline-calibration.json" || exit $?
cp "$BASELINE_SUMMARY" "$RUN_DIR/baseline-summary.json" || exit $?

TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"
  BUILD_PROFILE="external"
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
elif [[ "$PROFILE" == smoke ]]; then
  BIN="$TARGET_DIR/debug/kvbench"
  BUILD_PROFILE="debug"
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" \
    build --locked --features kv-all --bin kvbench || exit $?
else
  BIN="$TARGET_DIR/release/kvbench"
  BUILD_PROFILE="release"
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.99.sh" \
    build --release --locked --features kv-all --bin kvbench || exit $?
fi
BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}')
if [[ "${ALLOW_BUSY:-0}" != 1 ]]; then
  check_quiet_host post-build || {
    rc=$?
    echo "benchmark binary is now built; rerun once the host is quiet" >&2
    exit "$rc"
  }
fi

LEVELS_JSON=$(printf '%s\n' "${LEVELS[@]}" | jq -s 'map(tonumber)')
jq -n \
  --arg lane "io-contention" \
  --arg profile "$PROFILE" \
  --arg build_profile "$BUILD_PROFILE" \
  --arg binary_sha256 "$BIN_SHA" \
  --arg baseline_dir "$(realpath "$BASELINE_DIR")" \
  --arg baseline_read_file "$READ_CALIBRATION_FILE" \
  --arg baseline_write_file "$WRITE_CALIBRATION_FILE" \
  --arg baseline_read_sha256 "$READ_BASELINE_SHA" \
  --arg baseline_write_sha256 "$WRITE_BASELINE_SHA" \
  --arg baseline_support_sha256 "$BASELINE_SUPPORT_SHA" \
  --arg host_name "$CURRENT_HOST_NAME" \
  --arg machine_id_sha256 "$CURRENT_MACHINE_ID_SHA256" \
  --arg filesystem "$CURRENT_FSTYPE" \
  --arg source "$CURRENT_SOURCE" \
  --argjson calibration_protocol_version "$CALIBRATION_PROTOCOL_VERSION" \
  --argjson baseline_iops "$BASE_IOPS" \
  --argjson baseline_read_iops "$BASE_READ_IOPS" \
  --argjson baseline_write_iops "$BASE_WRITE_IOPS" \
  --argjson read_fraction "$READ_FRACTION" \
  --argjson write_fraction "$WRITE_FRACTION" \
  --argjson pressure_bs_bytes "$PRESSURE_BS_BYTES" \
  --argjson levels "$LEVELS_JSON" \
  '{
    lane:$lane,
    profile:$profile,
    build_profile:$build_profile,
    benchmark_binary_sha256:$binary_sha256,
    calibration_protocol_version:$calibration_protocol_version,
    baseline_dir:$baseline_dir,
    baseline_read_file:$baseline_read_file,
    baseline_write_file:$baseline_write_file,
    baseline_read_sha256:$baseline_read_sha256,
    baseline_write_sha256:$baseline_write_sha256,
    baseline_support_sha256:$baseline_support_sha256,
    host_name:$host_name,
    machine_id_sha256:$machine_id_sha256,
    filesystem:$filesystem,
    source:$source,
    baseline_iops:$baseline_iops,
    baseline_read_iops:$baseline_read_iops,
    baseline_write_iops:$baseline_write_iops,
    read_fraction:$read_fraction,
    write_fraction:$write_fraction,
    pressure_bs_bytes:$pressure_bs_bytes,
    pressure_levels_percent:$levels,
    pressure_method:"two independent psync fio workers on disjoint files: randread and randwrite, each using the aligned calibration block size and its directional IOPS cap",
    pressure_scope:"both pressure workers span the complete kvbench process invocation including open, prefill, warmup and measured phase",
    pressure_precondition_s:2,
    pressure_statistics:"each fio worker uses ramp_time=2 so delivered directional statistics begin with the database process interval",
    post_pressure_settle:"after each nonzero pressure case, stop both fio workers, sync, then sleep 0.5 s before another case",
    interpretation:"pressure percent is requested total IOPS relative to min(read_iops/read_fraction, write_iops/write_fraction); read and write caps and delivered rates remain independently inspectable"
  }' > "$RUN_DIR/support.json" || exit $?

PRESSURE_READ_FILE="$DATA_DIR/fio-pressure-read.bin"
PRESSURE_WRITE_FILE="$DATA_DIR/fio-pressure-write.bin"
mkdir -p "$RUN_DIR/pressure/raw"
fio --name=prepare-pressure-read --filename="$PRESSURE_READ_FILE" --size="$PRESSURE_SIZE" \
  --rw=write --bs="$PREP_BS_BYTES" --ioengine=psync --direct=1 --fsync_on_close=1 \
  --output-format=json > "$RUN_DIR/pressure-prepare-read.json" || exit $?
fio --name=prepare-pressure-write --filename="$PRESSURE_WRITE_FILE" --size="$PRESSURE_SIZE" \
  --rw=write --bs="$PREP_BS_BYTES" --ioengine=psync --direct=1 --fsync_on_close=1 \
  --output-format=json > "$RUN_DIR/pressure-prepare-write.json" || exit $?


ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv paritydb-hash paritydb-btree rocksdb mdbx persy roughdb jammdb lsmdb)
WORKLOADS=(point-read write-burst churn)
if [[ -n "${ENGINES_OVERRIDE:-}" ]]; then read -r -a ENGINES <<< "$ENGINES_OVERRIDE"; fi
if [[ -n "${WORKLOADS_OVERRIDE:-}" ]]; then read -r -a WORKLOADS <<< "$WORKLOADS_OVERRIDE"; fi
primary_durability() {
  case "$1" in
    paritydb-hash|paritydb-btree) echo relaxed ;;
    *) echo sync ;;
  esac
}

JOBS=()
for trial in $(seq 1 "$TRIALS"); do
  for pct in "${LEVELS[@]}"; do
    for workload in "${WORKLOADS[@]}"; do
      for engine in "${ENGINES[@]}"; do
        dur=$(primary_durability "$engine")
        JOBS+=("$trial|$pct|$workload|$engine|$dur")
      done
    done
  done
done
mapfile -t ORDERED < <(printf '%s\n' "${JOBS[@]}" | shuf)
printf '%s\n' "${ORDERED[@]}" > "$RUN_DIR/jobs.txt"

clear_failure() {
  local case_id=$1 failures="$RUN_DIR/failures.ndjson"
  [[ -f "$failures" ]] || return 0
  local tmp="${failures}.tmp"
  jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' "$failures" > "$tmp"
  mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}

record_failure() {
  local case_id=$1 stage=$2 rc=$3 stderr_path=$4
  clear_failure "$case_id"
  jq -cn \
    --arg case_id "$case_id" --arg stage "$stage" --arg stderr "$stderr_path" --argjson rc "$rc" \
    '{case_id:$case_id,stage:$stage,returncode:$rc,stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
}

pressure_read_pid=""
pressure_write_pid=""
normalize_fio_output() {
  local verbatim=$1
  local json=$2
  local tmp="${json}.tmp"
  [[ -s "$verbatim" ]] || return 1
  awk 'found || /^[[:space:]]*\{/ { found=1; print }' "$verbatim" > "$tmp"
  if jq -e . "$tmp" >/dev/null 2>&1; then
    mv "$tmp" "$json"
    return 0
  fi
  rm -f "$tmp" "$json"
  return 1
}
stop_pressure() {
  local pid
  for pid in "$pressure_read_pid" "$pressure_write_pid"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill -INT "$pid" 2>/dev/null || true
    fi
  done
  for pid in "$pressure_read_pid" "$pressure_write_pid"; do
    if [[ -n "$pid" ]]; then
      wait "$pid" 2>/dev/null || true
    fi
  done
  pressure_read_pid=""
  pressure_write_pid=""
}
cleanup() {
  stop_pressure
  rm -f -- "$PRESSURE_READ_FILE" "$PRESSURE_WRITE_FILE"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

INDEX=0
TOTAL=${#ORDERED[@]}
for job in "${ORDERED[@]}"; do
  INDEX=$((INDEX + 1))
  IFS='|' read -r trial pct workload engine dur <<< "$job"
  case_id="t${trial}-io${pct}pct-${engine}-${dur}-${workload}"
  out="$RUN_DIR/cases/$case_id.json"
  pressure_out="$RUN_DIR/pressure/$case_id.json"
  storage_out="$RUN_DIR/storage/$case_id.json"

  pressure_read_raw="$RUN_DIR/pressure/raw/$case_id-read.json"
  pressure_write_raw="$RUN_DIR/pressure/raw/$case_id-write.json"
  pressure_read_verbatim="$RUN_DIR/pressure/raw/$case_id-read.fio-output"
  pressure_write_verbatim="$RUN_DIR/pressure/raw/$case_id-write.fio-output"

  if [[ -s "$out" && -s "$pressure_out" && -s "$storage_out" ]]; then
    resume_evidence_ok=0
    if (( pct == 0 )); then
      if jq -e --argjson pct "$pct" --argjson bs "$PRESSURE_BS_BYTES" \
        '.pressure_percent == $pct and .bs_bytes == $bs and .target_iops == 0 and (.jobs | length) == 0' \
        "$pressure_out" >/dev/null 2>&1; then
        resume_evidence_ok=1
      fi
    elif jq -e --arg bs "$PRESSURE_BS_BYTES" --argjson pct "$pct" '
      .pressure_percent == $pct and .bs_bytes == ($bs | tonumber) and
      (.jobs | length) == 2 and
      ([.jobs[] | select(.error == 0 and .["job options"].rw == "randread" and .["job options"].bs == $bs and .["job options"].ba == $bs and .["job options"].ioengine == "psync" and .["job options"].direct == "1")] | length) == 1 and
      ([.jobs[] | select(.error == 0 and .["job options"].rw == "randwrite" and .["job options"].bs == $bs and .["job options"].ba == $bs and .["job options"].ioengine == "psync" and .["job options"].direct == "1")] | length) == 1
    ' "$pressure_out" >/dev/null 2>&1; then
      resume_evidence_ok=1
    fi
    if (( resume_evidence_ok == 1 )); then
      clear_failure "$case_id"
      continue
    fi
  fi
  rm -f "$out" "$pressure_out" "$storage_out" \
    "$pressure_read_raw" "$pressure_write_raw" "$pressure_read_verbatim" "$pressure_write_verbatim"
  clear_failure "$case_id"
  echo "[$INDEX/$TOTAL] $case_id" >&2

  pressure_read_pid=""
  pressure_write_pid=""
  target_total=0
  read_iops=0
  write_iops=0
  if (( pct > 0 )); then
    target_total=$(awk -v b="$BASE_IOPS" -v p="$pct" 'BEGIN { printf "%.0f", b*p/100.0 }')
    (( target_total < 2 )) && target_total=2
    read_iops=$(awk -v t="$target_total" -v f="$READ_FRACTION" 'BEGIN { printf "%.0f", t*f }')
    (( read_iops < 1 )) && read_iops=1
    write_iops=$((target_total - read_iops))
    (( write_iops < 1 )) && write_iops=1
    target_total=$((read_iops + write_iops))
    fio --name=pressure-read --filename="$PRESSURE_READ_FILE" --size="$PRESSURE_SIZE" \
      --rw=randread --bs="$PRESSURE_BS_BYTES" --blockalign="$PRESSURE_BS_BYTES" \
      --ioengine=psync --direct=1 --rate_iops="$read_iops" \
      --time_based=1 --runtime=86400 --ramp_time=2 --randrepeat=1 \
      --output-format=json --output="$pressure_read_verbatim" &
    pressure_read_pid=$!
    fio --name=pressure-write --filename="$PRESSURE_WRITE_FILE" --size="$PRESSURE_SIZE" \
      --rw=randwrite --bs="$PRESSURE_BS_BYTES" --blockalign="$PRESSURE_BS_BYTES" \
      --ioengine=psync --direct=1 --rate_iops="$write_iops" \
      --time_based=1 --runtime=86400 --ramp_time=2 --randrepeat=1 \
      --output-format=json --output="$pressure_write_verbatim" &
    pressure_write_pid=$!
    sleep 2
    if ! kill -0 "$pressure_read_pid" 2>/dev/null || ! kill -0 "$pressure_write_pid" 2>/dev/null; then
      stop_pressure
      record_failure "$case_id" pressure-start 1 "$RUN_DIR/pressure/raw"
      sync
      sleep 0.5
      continue
    fi
  else
    jq -cn \
      --argjson pressure_percent 0 --argjson target_iops 0 \
      --argjson target_read_iops 0 --argjson target_write_iops 0 \
      --argjson baseline_iops "$BASE_IOPS" \
      --argjson baseline_read_iops "$BASE_READ_IOPS" --argjson baseline_write_iops "$BASE_WRITE_IOPS" \
      --argjson read_fraction "$READ_FRACTION" --argjson write_fraction "$WRITE_FRACTION" \
      --argjson bs_bytes "$PRESSURE_BS_BYTES" \
      '{pressure_percent:$pressure_percent,target_iops:$target_iops,target_read_iops:$target_read_iops,target_write_iops:$target_write_iops,baseline_iops:$baseline_iops,baseline_read_iops:$baseline_read_iops,baseline_write_iops:$baseline_write_iops,read_fraction:$read_fraction,write_fraction:$write_fraction,bs_bytes:$bs_bytes,jobs:[]}' \
      > "$pressure_out"
  fi

  storage_before=$("$ROOT/scripts/capture-storage-counters.sh" "$DATA_DIR") || {
    stop_pressure
    record_failure "$case_id" storage-before 1 "$RUN_DIR/storage/$case_id.before.error"
    continue
  }

  err="$RUN_DIR/stderr/$case_id.log"
  "$BIN" --engine "$engine" --durability "$dur" --workload "$workload" \
    --records "$RECORDS" --ops "$OPS" --value-bytes 256 --txn-size 100 \
    --trial "$trial" --seed 1592606758 --scenario "io-pressure-${pct}pct" \
    --root "$DATA_DIR" --output "$out" 2>"$err"
  rc=$?
  storage_after=$("$ROOT/scripts/capture-storage-counters.sh" "$DATA_DIR")
  storage_rc=$?
  if (( storage_rc == 0 )); then
    jq -cn --argjson before "$storage_before" --argjson after "$storage_after"       '{before:$before,after:$after}' > "$storage_out"
  fi
  pressure_alive_after=1
  if (( pct > 0 )) && { ! kill -0 "$pressure_read_pid" 2>/dev/null || ! kill -0 "$pressure_write_pid" 2>/dev/null; }; then
    pressure_alive_after=0
  fi
  stop_pressure
  if (( pct > 0 )); then
    normalize_fio_output "$pressure_read_verbatim" "$pressure_read_raw" || true
    normalize_fio_output "$pressure_write_verbatim" "$pressure_write_raw" || true
  fi

  pressure_ok=1
  if (( pct > 0 )); then
    if (( pressure_alive_after == 0 )); then
      pressure_ok=0
    elif [[ ! -s "$pressure_read_raw" || ! -s "$pressure_write_raw" ]]; then
      pressure_ok=0
    elif ! jq -e --arg bs "$PRESSURE_BS_BYTES" '
      (.jobs | length) == 1 and (.jobs[0].error // 0) == 0 and
      .jobs[0]["job options"].rw == "randread" and .jobs[0]["job options"].bs == $bs and
      .jobs[0]["job options"].ba == $bs and .jobs[0]["job options"].ioengine == "psync" and .jobs[0]["job options"].direct == "1"
    ' "$pressure_read_raw" >/dev/null 2>&1; then
      pressure_ok=0
    elif ! jq -e --arg bs "$PRESSURE_BS_BYTES" '
      (.jobs | length) == 1 and (.jobs[0].error // 0) == 0 and
      .jobs[0]["job options"].rw == "randwrite" and .jobs[0]["job options"].bs == $bs and
      .jobs[0]["job options"].ba == $bs and .jobs[0]["job options"].ioengine == "psync" and .jobs[0]["job options"].direct == "1"
    ' "$pressure_write_raw" >/dev/null 2>&1; then
      pressure_ok=0
    else
      if ! jq -cn \
        --slurpfile read_raw "$pressure_read_raw" --slurpfile write_raw "$pressure_write_raw" \
        --arg read_raw_file "$(basename "$pressure_read_raw")" --arg write_raw_file "$(basename "$pressure_write_raw")" \
        --arg read_verbatim_file "$(basename "$pressure_read_verbatim")" --arg write_verbatim_file "$(basename "$pressure_write_verbatim")" \
        --argjson pressure_percent "$pct" --argjson target_iops "$target_total" \
        --argjson target_read_iops "$read_iops" --argjson target_write_iops "$write_iops" \
        --argjson baseline_iops "$BASE_IOPS" --argjson baseline_read_iops "$BASE_READ_IOPS" \
        --argjson baseline_write_iops "$BASE_WRITE_IOPS" --argjson read_fraction "$READ_FRACTION" \
        --argjson write_fraction "$WRITE_FRACTION" --argjson bs_bytes "$PRESSURE_BS_BYTES" \
        '{pressure_percent:$pressure_percent,target_iops:$target_iops,target_read_iops:$target_read_iops,target_write_iops:$target_write_iops,baseline_iops:$baseline_iops,baseline_read_iops:$baseline_read_iops,baseline_write_iops:$baseline_write_iops,read_fraction:$read_fraction,write_fraction:$write_fraction,bs_bytes:$bs_bytes,jobs:[$read_raw[0].jobs[0],$write_raw[0].jobs[0]],raw_files:{read_json:$read_raw_file,write_json:$write_raw_file,read_fio_output:$read_verbatim_file,write_fio_output:$write_verbatim_file}}' \
        > "$pressure_out"; then
        pressure_ok=0
      fi
    fi
    sync
    sleep 0.5
  fi


  if (( rc != 0 )); then
    rm -f "$out"
    record_failure "$case_id" benchmark "$rc" "$err"
  elif (( storage_rc != 0 )) || [[ ! -s "$storage_out" ]]; then
    rm -f "$out"
    record_failure "$case_id" storage-evidence "${storage_rc:-1}" "$storage_out"
  elif (( pressure_ok == 0 )); then
    rm -f "$out"
    record_failure "$case_id" pressure-evidence 1 "$pressure_out"
  else
    clear_failure "$case_id"
    [[ -s "$err" ]] || rm -f "$err"
  fi
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT" || exit $?
uv run --script "$ROOT/scripts/summarize.py" "$RUN_DIR/results.ndjson" \
  --expect-trials "$TRIALS" --json-out "$RUN_DIR/summary.json" --markdown-out "$RUN_DIR/summary.md"
summary_rc=$?
uv run --script "$ROOT/scripts/summarize-io-pressure.py" "$RUN_DIR" \
  --expect-trials "$TRIALS" --json-out "$RUN_DIR/pressure-summary.json" \
  --markdown-out "$RUN_DIR/pressure-summary.md"
pressure_summary_rc=$?
if [[ -s "$RUN_DIR/failures.ndjson" ]]; then
  FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson")
else
  FAILURES=0
fi
COMPLETED=$(find "$RUN_DIR/cases" -type f -name '*.json' | wc -l)
printf 'run=%s baseline_iops=%s read_iops=%s write_iops=%s pressure_bs=%s total=%s completed=%s failures=%s summary_rc=%s pressure_summary_rc=%s results=%s\n' \
  "$RUN_ID" "$BASE_IOPS" "$BASE_READ_IOPS" "$BASE_WRITE_IOPS" "$PRESSURE_BS_BYTES" "$TOTAL" "$COMPLETED" "$FAILURES" \
  "$summary_rc" "$pressure_summary_rc" "$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 || summary_rc != 0 || pressure_summary_rc != 0 ? 1 : 0 ))
