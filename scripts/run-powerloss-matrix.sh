#!/usr/bin/env bash
set -u -o pipefail

PROFILE=${1:-quick}
BENCH_KIND=${BENCH_KIND:-kv}
case "$BENCH_KIND" in
  kv)
    BENCH_NAME=kvbench
    BENCH_FEATURES=kv-all
    BENCH_DATA_ARGS=(--value-bytes 256 --scan-len 100)
    DEFAULT_ENGINES=(redb fjall surrealkv heed sled lkv manifold turbokv rocksdb mdbx persy roughdb jammdb lsmdb)
    ;;
  record)
    BENCH_NAME=recordbench
    BENCH_FEATURES=record
    BENCH_DATA_ARGS=(--payload-bytes 256)
    DEFAULT_ENGINES=(surrealdb turso sqlite)
    ;;
  *)
    echo "BENCH_KIND must be kv or record" >&2
    exit 2
    ;;
esac

case "$PROFILE" in
  smoke)
    TRIALS=1; RECORDS=1000; DELAYS=(0.005); TXNS=(16)
    IMAGE_SIZE=2G; LOG_SIZE=512M; MIN_FREE_BYTES=$((8 * 1024 * 1024 * 1024))
    ;;
  quick)
    TRIALS=2; RECORDS=10000; DELAYS=(0.002 0.020 0.100); TXNS=(1 16 100)
    IMAGE_SIZE=4G; LOG_SIZE=1G; MIN_FREE_BYTES=$((20 * 1024 * 1024 * 1024))
    ;;
  full)
    TRIALS=5; RECORDS=100000; DELAYS=(0.001 0.005 0.020 0.100 0.500); TXNS=(1 16 100 1024)
    IMAGE_SIZE=8G; LOG_SIZE=2G; MIN_FREE_BYTES=$((40 * 1024 * 1024 * 1024))
    ;;
  *)
    echo "usage: $0 [smoke|quick|full]" >&2
    exit 2
    ;;
esac

ROOT=${ROOT:-/srv/scratch/db-bench-2026-09-27}
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dT%H%M%SZ)-powerloss-$BENCH_KIND-$PROFILE"}
RUN_DIR="$ROOT/results/runs/$RUN_ID"
DATA_DIR="$ROOT/data/runs/$RUN_ID"
BASE_DIR="$DATA_DIR/powerloss-bases"
WORK_ROOT="$DATA_DIR/powerloss-work"
MOUNT_DIR="$WORK_ROOT/mount"
mkdir -p "$RUN_DIR"/{cases,stderr,powerloss,progress,child} "$BASE_DIR" "$WORK_ROOT" "$MOUNT_DIR"

for cmd in sudo dmsetup losetup mkfs.ext4 mount umount mountpoint blockdev e2fsck jq sha256sum cp timeout; do
  command -v "$cmd" >/dev/null || { echo "missing required command: $cmd" >&2; exit 2; }
done
sudo -n true 2>/dev/null || {
  echo "power-loss lane requires root block-device control; configure sudo or run it as root after building the benchmark as the normal user" >&2
  exit 77
}
sudo -n modprobe dm-log-writes || exit $?
sudo -n dmsetup targets | grep -q '^log-writes ' || {
  echo "dm-log-writes target is unavailable after modprobe" >&2
  exit 77
}

TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/rust-db-realistic-bench-target}
if (( EUID == 0 )) && [[ -z "${BENCH_BIN:-}" ]]; then
  echo "do not build the benchmark as root; build as the normal user and pass BENCH_BIN when invoking this lane as root" >&2
  exit 77
fi
if [[ -n "${BENCH_BIN:-}" ]]; then
  BIN="$BENCH_BIN"
  [[ -x "$BIN" ]] || { echo "BENCH_BIN is not executable: $BIN" >&2; exit 2; }
else
  BIN="$TARGET_DIR/release/$BENCH_NAME"
  CARGO_TARGET_DIR="$TARGET_DIR" "$ROOT/scripts/cargo-local-1.98.1.sh" \
    build --release --features "$BENCH_FEATURES" --bin "$BENCH_NAME" || exit $?
fi
BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}')

if [[ -n "${REPLAY_LOG:-}" ]]; then
  REPLAY="$REPLAY_LOG"
else
  REPLAY=$("$ROOT/scripts/ensure-log-writes.sh") || exit $?
fi
[[ -x "$REPLAY" ]] || { echo "replay-log is not executable: $REPLAY" >&2; exit 2; }
REPLAY_COMMIT=$(git -C "$(dirname "$REPLAY")" rev-parse HEAD 2>/dev/null || echo external)

available=$(df -B1 --output=avail "$WORK_ROOT" | tail -n1 | tr -d ' ')
if (( available < MIN_FREE_BYTES )); then
  echo "refusing power-loss run: only $available bytes free; profile requires at least $MIN_FREE_BYTES bytes headroom" >&2
  exit 75
fi

ENGINES=("${DEFAULT_ENGINES[@]}")
if [[ -n "${ENGINES_OVERRIDE:-}" ]]; then
  read -r -a ENGINES <<< "$ENGINES_OVERRIDE"
elif [[ "$BENCH_KIND" == kv && "${INCLUDE_KNOWN_BROKEN:-0}" == 1 ]]; then
  ENGINES+=(manifold-wal)
fi

"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-start.txt" "$ROOT"
DM_VERSION=$(sudo -n dmsetup targets | awk '$1=="log-writes"{print $2; exit}')
cat > "$RUN_DIR/support.json" <<JSON
{
  "lane": "simulated-power-loss",
  "benchmark_kind": "$BENCH_KIND",
  "benchmark_binary": "$BENCH_NAME",
  "profile": "$PROFILE",
  "filesystem": "ext4 on disposable file-backed loop device",
  "image_size": "$IMAGE_SIZE",
  "log_size": "$LOG_SIZE",
  "cut_method": "SIGSTOP writer, record acknowledged prefix, dmsetup suspend --noflush, insert/wait for dm-log-writes mark barrier, snapshot log, then SIGKILL",
  "durable_image_model": "replay the complete dm-log-writes log through the post-suspend mark onto the pristine base image; ordinary writes remain excluded until a PREFLUSH proves them stable, FUA writes are included after completion, and the mark drains dm-log-writes' asynchronous logger without promoting unflushed DB writes",
  "dm_log_writes_version": "$DM_VERSION",
  "replay_log_commit": "$REPLAY_COMMIT",
  "benchmark_binary_sha256": "$BIN_SHA",
  "known_broken_opt_in": ${INCLUDE_KNOWN_BROKEN:-0}
}
JSON

BASE_LOOP=
BASE_MNT=
DATA_LOOP=
LOG_LOOP=
SNAP_LOG_LOOP=
REPLAY_LOOP=
DM_NAME=
ACTIVE_MNT=
REPLAY_MNT=
WRITER_PID=

cleanup_case() {
  set +e
  if [[ -n "$WRITER_PID" ]]; then
    kill -KILL "$WRITER_PID" 2>/dev/null || true
  fi
  if [[ -n "$DM_NAME" ]] && sudo -n dmsetup info "$DM_NAME" >/dev/null 2>&1; then
    sudo -n dmsetup resume "$DM_NAME" >/dev/null 2>&1 || true
  fi
  if [[ -n "$WRITER_PID" ]]; then
    wait "$WRITER_PID" 2>/dev/null || true
  fi
  [[ -n "$REPLAY_MNT" ]] && mountpoint -q "$REPLAY_MNT" && sudo -n umount "$REPLAY_MNT" >/dev/null 2>&1 || true
  [[ -n "$ACTIVE_MNT" ]] && mountpoint -q "$ACTIVE_MNT" && sudo -n umount "$ACTIVE_MNT" >/dev/null 2>&1 || true
  [[ -n "$BASE_MNT" ]] && mountpoint -q "$BASE_MNT" && sudo -n umount "$BASE_MNT" >/dev/null 2>&1 || true
  if [[ -n "$DM_NAME" ]]; then sudo -n dmsetup remove --retry "$DM_NAME" >/dev/null 2>&1 || true; fi
  [[ -n "$REPLAY_LOOP" ]] && sudo -n losetup -d "$REPLAY_LOOP" >/dev/null 2>&1 || true
  [[ -n "$SNAP_LOG_LOOP" ]] && sudo -n losetup -d "$SNAP_LOG_LOOP" >/dev/null 2>&1 || true
  [[ -n "$LOG_LOOP" ]] && sudo -n losetup -d "$LOG_LOOP" >/dev/null 2>&1 || true
  [[ -n "$DATA_LOOP" ]] && sudo -n losetup -d "$DATA_LOOP" >/dev/null 2>&1 || true
  [[ -n "$BASE_LOOP" ]] && sudo -n losetup -d "$BASE_LOOP" >/dev/null 2>&1 || true
  BASE_LOOP=; BASE_MNT=; DATA_LOOP=; LOG_LOOP=; SNAP_LOG_LOOP=; REPLAY_LOOP=; DM_NAME=; ACTIVE_MNT=; REPLAY_MNT=; WRITER_PID=
  set -u -o pipefail
}
trap cleanup_case EXIT
trap 'cleanup_case; exit 130' INT
trap 'cleanup_case; exit 143' TERM

clear_failure() {
  local case_id=$1
  local failures="$RUN_DIR/failures.ndjson"
  [[ -f "$failures" ]] || return 0
  local tmp="${failures}.tmp"
  jq -c --arg case_id "$case_id" 'select(.case_id != $case_id)' "$failures" > "$tmp"
  mv "$tmp" "$failures"
  [[ -s "$failures" ]] || rm -f "$failures"
}

record_failure() {
  local case_id=$1 stage=$2 rc=$3 stderr=$4
  clear_failure "$case_id"
  jq -cn --arg case_id "$case_id" --arg stage "$stage" --arg stderr "$stderr" --argjson rc "$rc" \
    '{case_id:$case_id, stage:$stage, returncode:$rc, stderr:$stderr}' >> "$RUN_DIR/failures.ndjson"
}

PREPARED_BASE=
prepare_base() {
  local engine=$1 trial=$2
  local base_img="$BASE_DIR/t${trial}-${engine}.img"
  local meta="$BASE_DIR/t${trial}-${engine}.json"
  local expected
  expected=$(jq -cn --arg engine "$engine" --arg kind "$BENCH_KIND" --argjson trial "$trial" --argjson records "$RECORDS" \
    --arg image_size "$IMAGE_SIZE" --arg bin_sha "$BIN_SHA" --arg mount_path "$MOUNT_DIR" \
    '{engine:$engine,benchmark_kind:$kind,trial:$trial,records:$records,image_size:$image_size,binary_sha256:$bin_sha,mount_path:$mount_path,seed:1592606758,datum_bytes:256}')
  if [[ -s "$base_img" && -s "$meta" ]] && [[ "$(jq -cS . "$meta")" == "$(printf '%s' "$expected" | jq -cS .)" ]]; then
    PREPARED_BASE="$base_img"
    return 0
  fi

  rm -f "$base_img" "$meta"
  truncate -s "$IMAGE_SIZE" "$base_img"
  mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 "$base_img" || return $?
  BASE_LOOP=$(sudo -n losetup --find --show "$base_img") || return $?
  local mnt="$MOUNT_DIR"
  BASE_MNT="$mnt"
  mountpoint -q "$mnt" && { echo "shared power-loss mountpoint unexpectedly busy: $mnt" >&2; return 16; }
  mkdir -p "$mnt"
  sudo -n mount -o noatime "$BASE_LOOP" "$mnt" || return $?
  sudo -n chown "$(id -u):$(id -g)" "$mnt" || return $?

  "$BIN" --engine "$engine" --durability sync --workload point-read \
    --records "$RECORDS" --ops 1 "${BENCH_DATA_ARGS[@]}" --txn-size 16 \
    --trial "$trial" --seed 1592606758 --scenario powerloss-prepare --db-name db \
    --root "$mnt" --output /dev/null --keep-db --warmup-reads 0 \
    >"$RUN_DIR/stderr/base-t${trial}-${engine}.stdout" \
    2>"$RUN_DIR/stderr/base-t${trial}-${engine}.stderr"
  local rc=$?
  if (( rc != 0 )); then return "$rc"; fi

  sync
  sudo -n umount "$mnt" || return $?
  BASE_MNT=
  sudo -n losetup -d "$BASE_LOOP" || return $?
  BASE_LOOP=
  rm -rf "$mnt"
  printf '%s\n' "$expected" > "$meta"
  PREPARED_BASE="$base_img"
}

TOTAL=0
INDEX=0
for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    for txn in "${TXNS[@]}"; do
      for delay in "${DELAYS[@]}"; do
        TOTAL=$((TOTAL + 1))
      done
    done
  done
done

for trial in $(seq 1 "$TRIALS"); do
  for engine in "${ENGINES[@]}"; do
    PREPARED_BASE=
    prepare_base "$engine" "$trial"
    rc=$?
    base_img="$PREPARED_BASE"
    if (( rc != 0 )); then
      echo "base preparation failed for $engine trial=$trial rc=$rc" >&2
      for txn in "${TXNS[@]}"; do
        for delay in "${DELAYS[@]}"; do
          tag=$(printf '%s' "$delay" | tr '.' 'p')
          case_id="t${trial}-${engine}-sync-tx${txn}-d${tag}"
          record_failure "$case_id" base-prepare "$rc" "$RUN_DIR/stderr/base-t${trial}-${engine}.stderr"
        done
      done
      cleanup_case
      continue
    fi

    for txn in "${TXNS[@]}"; do
      for delay in "${DELAYS[@]}"; do
        INDEX=$((INDEX + 1))
        tag=$(printf '%s' "$delay" | tr '.' 'p')
        case_id="t${trial}-${engine}-sync-tx${txn}-d${tag}"
        out="$RUN_DIR/cases/$case_id.json"
        if [[ -s "$out" ]]; then
          clear_failure "$case_id"
          continue
        fi

        echo "[$INDEX/$TOTAL] $case_id" >&2
        work="$WORK_ROOT/$case_id"
        rm -rf "$work"
        mkdir -p "$work" "$MOUNT_DIR"
        mountpoint -q "$MOUNT_DIR" && { record_failure "$case_id" stale-mount 16 "$RUN_DIR/stderr/$case_id.mount.log"; cleanup_case; continue; }
        ACTIVE_MNT="$MOUNT_DIR"
        REPLAY_MNT="$MOUNT_DIR"
        data_img="$work/data.img"
        replay_img="$work/replay.img"
        log_img="$work/log.img"
        cp --reflink=auto --sparse=always "$base_img" "$data_img"
        cp --reflink=auto --sparse=always "$base_img" "$replay_img"
        truncate -s "$LOG_SIZE" "$log_img"

        DATA_LOOP=$(sudo -n losetup --find --show "$data_img")
        LOG_LOOP=$(sudo -n losetup --find --show "$log_img")
        sectors=$(sudo -n blockdev --getsz "$DATA_LOOP")
        DM_NAME="dbpl-$$-$INDEX"
        sudo -n dmsetup create "$DM_NAME" --table "0 $sectors log-writes $DATA_LOOP $LOG_LOOP"
        rc=$?
        if (( rc != 0 )); then record_failure "$case_id" dm-create "$rc" "$RUN_DIR/stderr/$case_id.dm.log"; cleanup_case; rm -rf "$work"; continue; fi
        sudo -n mount -o noatime "/dev/mapper/$DM_NAME" "$ACTIVE_MNT"
        rc=$?
        if (( rc != 0 )); then record_failure "$case_id" active-mount "$rc" "$RUN_DIR/stderr/$case_id.mount.log"; cleanup_case; rm -rf "$work"; continue; fi

        progress="$RUN_DIR/progress/$case_id.txt"
        child_out="$RUN_DIR/child/$case_id.out"
        child_err="$RUN_DIR/stderr/$case_id.child.log"
        verify_err="$RUN_DIR/stderr/$case_id.verify.log"
        fsck_out="$RUN_DIR/powerloss/$case_id.e2fsck.txt"
        rm -f "$progress" "$child_out"

        "$BIN" --engine "$engine" --durability sync --workload write-burst \
          --records "$RECORDS" --ops 1000000000 "${BENCH_DATA_ARGS[@]}" --txn-size "$txn" \
          --trial "$trial" --seed 1592606758 --scenario powerloss-child --db-name db \
          --root "$ACTIVE_MNT" --output "$child_out" --keep-db --reuse-db --skip-prefill \
          --warmup-reads 0 --progress-file "$progress" >/dev/null 2>"$child_err" &
        WRITER_PID=$!

        ready=0
        for _ in $(seq 1 10000); do
          if [[ -s "$progress" ]]; then ready=1; break; fi
          if ! kill -0 "$WRITER_PID" 2>/dev/null; then break; fi
          sleep 0.001
        done
        if (( ready == 0 )); then
          record_failure "$case_id" writer-ready 1 "$child_err"
          cleanup_case; rm -rf "$work"; continue
        fi

        sleep "$delay"

        # Freeze userspace first so the acknowledged-prefix marker cannot race
        # ahead while dmsetup is quiescing the block target. If the writer is
        # inside a commit, SIGSTOP becomes effective before it can advance the
        # userspace progress marker again.
        freeze_start_ns=$(date +%s%N)
        kill -STOP "$WRITER_PID" 2>/dev/null || true
        stopped=0
        for _ in $(seq 1 5000); do
          stat=$(ps -o stat= -p "$WRITER_PID" 2>/dev/null | tr -d ' ')
          if [[ "$stat" == T* || "$stat" == t* ]]; then stopped=1; break; fi
          [[ -z "$stat" ]] && break
          sleep 0.001
        done
        writer_stopped_ns=$(date +%s%N)
        if (( stopped == 0 )); then
          record_failure "$case_id" writer-stop 1 "$child_err"
          cleanup_case; rm -rf "$work"; continue
        fi

        acked=$(tr -dc '0-9' < "$progress")
        [[ -n "$acked" ]] || acked=0
        cut_start_ns=$(date +%s%N)
        sudo -n dmsetup suspend --noflush "$DM_NAME"
        rc=$?
        cut_end_ns=$(date +%s%N)
        if (( rc != 0 )); then
          record_failure "$case_id" dm-suspend "$rc" "$RUN_DIR/stderr/$case_id.dm.log"
          cleanup_case; rm -rf "$work"; continue
        fi

        # The suspended dm target is now the exact cut boundary. dm-log-writes
        # completes database bios before its separate log kthread necessarily
        # persists the corresponding log record/superblock. Insert a mark after
        # quiescing the target and wait until replay-log can see it. This drains
        # only already-durable/FUA logging work; it does NOT splice unflushed
        # ordinary writes into the durable log.
        dm_status=$(sudo -n dmsetup status "$DM_NAME" 2>/dev/null || true)
        pre_mark_entries=$(awk '{print $4}' <<< "$dm_status")
        cut_mark="cut-${case_id}-${cut_end_ns}"
        mark_stdout="$RUN_DIR/powerloss/$case_id.mark.stdout"
        mark_stderr="$RUN_DIR/powerloss/$case_id.mark.stderr"
        mark_diag="$RUN_DIR/powerloss/$case_id.mark-diagnostic.txt"
        sudo -n dmsetup message "$DM_NAME" 0 mark "$cut_mark"           >"$mark_stdout" 2>"$mark_stderr"
        rc=$?
        if (( rc != 0 )); then
          record_failure "$case_id" log-mark "$rc" "$mark_stderr"
          cleanup_case; rm -rf "$work"; continue
        fi

        # Avoid repeatedly rescanning a growing log. The kernel increments the
        # target's logged_entries before writing a record, while LOG_MARK causes
        # log_super() to write+wait for a superblock containing the new count.
        # Once the raw super count catches the target count, try a single mark
        # lookup. If that equality was only an earlier FUA checkpoint, the mark
        # lookup fails and we continue waiting.
        mark_entry=
        header_entries=0
        status_entries=$pre_mark_entries
        expected_magic=$((0x6a736677736872))
        for _ in $(seq 1 3000); do
          status_now=$(sudo -n dmsetup status "$DM_NAME" 2>/dev/null || true)
          status_entries=$(awk '{print $4}' <<< "$status_now")
          if [[ "$status_now" == *logging_disabled* ]]; then
            break
          fi
          header_magic=$(od -An -tu8 -N8 "$LOG_LOOP" 2>/dev/null | tr -d '[:space:]')
          header_entries=$(od -An -tu8 -j16 -N8 "$LOG_LOOP" 2>/dev/null | tr -d '[:space:]')
          [[ -n "$header_magic" ]] || header_magic=0
          [[ -n "$header_entries" ]] || header_entries=0
          if (( header_magic == expected_magic && status_entries > pre_mark_entries && header_entries == status_entries )); then
            mark_entry=$("$REPLAY" --log "$LOG_LOOP" --find --end-mark "$cut_mark" 2>/dev/null) && break
            mark_entry=
          fi
          sleep 0.01
        done
        printf 'pre_mark_entries=%s
status_entries=%s
header_entries=%s
status=%s
mark=%s
'           "$pre_mark_entries" "$status_entries" "$header_entries" "$status_now" "$cut_mark" > "$mark_diag"
        if [[ -z "$mark_entry" ]]; then
          record_failure "$case_id" log-drain 1 "$mark_diag"
          cleanup_case; rm -rf "$work"; continue
        fi

        log_entries=$("$REPLAY" --log "$LOG_LOOP" --num-entries           2>"$RUN_DIR/stderr/$case_id.replay-info.log")
        rc=$?
        if (( rc != 0 )); then
          record_failure "$case_id" log-inspect "$rc" "$RUN_DIR/stderr/$case_id.replay-info.log"
          cleanup_case; rm -rf "$work"; continue
        fi

        log_snapshot="$work/log-at-cut.img"
        cp --reflink=auto --sparse=always "$log_img" "$log_snapshot"
        rc=$?
        if (( rc != 0 )); then
          record_failure "$case_id" log-snapshot "$rc" "$RUN_DIR/stderr/$case_id.replay-info.log"
          cleanup_case; rm -rf "$work"; continue
        fi

        kill -KILL "$WRITER_PID" 2>/dev/null || true
        sudo -n dmsetup resume "$DM_NAME"
        rc=$?
        if (( rc != 0 )); then
          record_failure "$case_id" dm-resume "$rc" "$RUN_DIR/stderr/$case_id.dm.log"
          cleanup_case; rm -rf "$work"; continue
        fi
        wait "$WRITER_PID" 2>/dev/null || true
        WRITER_PID=

        timeout 60s sudo -n umount "$ACTIVE_MNT"           >"$RUN_DIR/powerloss/$case_id.live-umount.stdout"           2>"$RUN_DIR/powerloss/$case_id.live-umount.stderr"
        live_umount_rc=$?
        if (( live_umount_rc != 0 )); then
          record_failure "$case_id" live-umount "$live_umount_rc" "$RUN_DIR/powerloss/$case_id.live-umount.stderr"
          cleanup_case; rm -rf "$work"; continue
        fi
        ACTIVE_MNT=

        sudo -n dmsetup remove --retry "$DM_NAME"
        rc=$?
        if (( rc != 0 )); then
          record_failure "$case_id" dm-remove "$rc" "$RUN_DIR/stderr/$case_id.dm.log"
          cleanup_case; rm -rf "$work"; continue
        fi
        DM_NAME=
        sudo -n losetup -d "$DATA_LOOP"; DATA_LOOP=
        sudo -n losetup -d "$LOG_LOOP"; LOG_LOOP=

        SNAP_LOG_LOOP=$(sudo -n losetup --find --show "$log_snapshot")
        REPLAY_LOOP=$(sudo -n losetup --find --show "$replay_img")
        timeout 120s "$REPLAY" --log "$SNAP_LOG_LOOP" --replay "$REPLAY_LOOP"           >"$RUN_DIR/powerloss/$case_id.replay.stdout"           2>"$RUN_DIR/powerloss/$case_id.replay.stderr"
        replay_rc=$?
        if (( replay_rc != 0 )); then
          record_failure "$case_id" replay "$replay_rc" "$RUN_DIR/powerloss/$case_id.replay.stderr"
          cleanup_case; rm -rf "$work"; continue
        fi

        timeout 60s sudo -n mount -o noatime "$REPLAY_LOOP" "$REPLAY_MNT"           >"$RUN_DIR/powerloss/$case_id.mount.stdout"           2>"$RUN_DIR/powerloss/$case_id.mount.stderr"
        mount_rc=$?
        if (( mount_rc != 0 )); then
          record_failure "$case_id" recovered-mount "$mount_rc" "$RUN_DIR/powerloss/$case_id.mount.stderr"
          cleanup_case; rm -rf "$work"; continue
        fi

        expected=$((RECORDS + acked))
        tail=$((txn * 2)); (( tail < 16 )) && tail=16
        timeout 120s "$BIN" --engine "$engine" --durability sync --workload point-read \
          --records "$RECORDS" --ops 1 "${BENCH_DATA_ARGS[@]}" --txn-size "$txn" \
          --trial "$trial" --seed 1592606758 --scenario "powerloss-dm-log-writes-d$tag" --db-name db \
          --root "$REPLAY_MNT" --output "$out" --keep-db --reuse-db --skip-prefill --warmup-reads 0 \
          --verify-prefix-records "$expected" --verify-tail-records "$tail" 2>"$verify_err"
        verify_rc=$?

        sudo -n umount "$REPLAY_MNT" >/dev/null 2>&1
        umount_rc=$?
        if (( umount_rc == 0 )); then REPLAY_MNT=; fi
        timeout 120s sudo -n e2fsck -f -n "$REPLAY_LOOP" >"$fsck_out" 2>&1
        fsck_rc=$?

        jq -cn \
          --arg case_id "$case_id" --arg engine "$engine" --arg benchmark_kind "$BENCH_KIND" --argjson trial "$trial" \
          --argjson txn "$txn" --arg delay "$delay" --argjson acked "$acked" \
          --argjson expected "$expected" --argjson log_entries "$log_entries" --argjson mark_entry "$mark_entry" \
          --arg dm_status "$dm_status" --argjson freeze_start_ns "$freeze_start_ns" \
          --argjson writer_stopped_ns "$writer_stopped_ns" --argjson cut_start_ns "$cut_start_ns" \
          --argjson cut_end_ns "$cut_end_ns" --argjson replay_rc "$replay_rc" \
          --argjson mount_rc "$mount_rc" --argjson verify_rc "$verify_rc" \
          --argjson live_umount_rc "$live_umount_rc"           --argjson recovered_umount_rc "$umount_rc" --argjson e2fsck_rc "$fsck_rc" \
          '{case_id:$case_id,engine:$engine,benchmark_kind:$benchmark_kind,trial:$trial,txn_size:$txn,kill_delay_s:$delay,
            acknowledged_ops:$acked,expected_prefix_records:$expected,log_entries:$log_entries,
            logger_mark_entry:$mark_entry,dm_status:$dm_status,
            writer_stop_ns:($writer_stopped_ns-$freeze_start_ns),cut_suspend_ns:($cut_end_ns-$cut_start_ns),
            replay_rc:$replay_rc,live_umount_rc:$live_umount_rc,
            recovered_mount_rc:$mount_rc,verify_rc:$verify_rc,
            recovered_umount_rc:$recovered_umount_rc,e2fsck_rc:$e2fsck_rc}' \
          > "$RUN_DIR/powerloss/$case_id.json"

        if (( verify_rc != 0 )) || [[ ! -s "$out" ]]; then
          rm -f "$out"
          record_failure "$case_id" verify "$verify_rc" "$verify_err"
        elif ! jq -e '.verification.prefix_present_after_gap == 0 and .verification.tail_present_after_gap == 0 and .verification.transaction_atomic_tail == true' "$out" >/dev/null; then
          record_failure "$case_id" structural-verification 1 "$verify_err"
        elif ! jq -e '.verification.verification_ok == true' "$out" >/dev/null; then
          record_failure "$case_id" durable-verification 1 "$verify_err"
        elif (( fsck_rc != 0 )); then
          record_failure "$case_id" filesystem-check "$fsck_rc" "$fsck_out"
        else
          clear_failure "$case_id"
        fi

        cleanup_case
        rm -rf "$work"
      done
    done
  done
done

find "$RUN_DIR/cases" -type f -name '*.json' -print0 | sort -z | xargs -0 -r cat > "$RUN_DIR/results.ndjson"
"$ROOT/scripts/capture-host-metadata.sh" "$RUN_DIR/host-end.txt" "$ROOT"
if [[ -s "$RUN_DIR/failures.ndjson" ]]; then FAILURES=$(wc -l < "$RUN_DIR/failures.ndjson"); else FAILURES=0; fi
echo "run=$RUN_ID cases=$TOTAL failures=$FAILURES results=$RUN_DIR/results.ndjson"
exit $(( FAILURES > 0 ? 1 : 0 ))
