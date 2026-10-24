#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'HELP'
usage: run-storage-envelope.sh PROFILE.json -- COMMAND [ARG...]

Validate the current host/storage stack against PROFILE.json and run COMMAND.
"mode":"native" only validates/exports provenance.
"mode":"bounded" additionally requires root and runs COMMAND in a transient
systemd service with the profile's CPU/memory/swap/block-I/O ceilings.
HELP
  exit 2
}

[[ $# -ge 3 ]] || usage
PROFILE=$1
shift
[[ ${1:-} == -- ]] || usage
shift
[[ $# -ge 1 ]] || usage

for tool in jq findmnt realpath sha256sum hostname; do
  command -v "$tool" >/dev/null 2>&1 || { echo "required tool not found: $tool" >&2; exit 127; }
done
[[ -s "$PROFILE" ]] || { echo "profile not found or empty: $PROFILE" >&2; exit 2; }
PROFILE=$(realpath "$PROFILE")

if ! jq -e '.format_version == 1 and (.id|type=="string" and length>0) and (.mode=="native" or .mode=="bounded") and (.root|type=="string" and length>0) and (.expected|type=="object")' "$PROFILE" >/dev/null; then
  echo "invalid storage profile header: $PROFILE" >&2
  exit 2
fi

PROFILE_ID=$(jq -r '.id' "$PROFILE")
MODE=$(jq -r '.mode' "$PROFILE")
ROOT_PATH=$(jq -r '.root' "$PROFILE")
[[ -e "$ROOT_PATH" ]] || { echo "storage profile root does not exist: $ROOT_PATH" >&2; exit 75; }
ROOT_PATH=$(realpath "$ROOT_PATH")
FSTYPE=$(findmnt -T "$ROOT_PATH" -n -o FSTYPE 2>/dev/null || true)
SOURCE=$(findmnt -T "$ROOT_PATH" -n -o SOURCE 2>/dev/null || true)
OPTIONS=$(findmnt -T "$ROOT_PATH" -n -o OPTIONS 2>/dev/null || true)
HOST=$(hostname)

EXPECTED_HOST=$(jq -r '.expected.hostname // empty' "$PROFILE")
EXPECTED_FSTYPE=$(jq -r '.expected.filesystem // empty' "$PROFILE")
EXPECTED_SOURCE=$(jq -r '.expected.source // empty' "$PROFILE")

[[ -z "$EXPECTED_HOST" || "$HOST" == "$EXPECTED_HOST" ]] || { echo "storage profile host mismatch: expected=$EXPECTED_HOST actual=$HOST" >&2; exit 75; }
[[ -z "$EXPECTED_FSTYPE" || "$FSTYPE" == "$EXPECTED_FSTYPE" ]] || { echo "storage profile filesystem mismatch: expected=$EXPECTED_FSTYPE actual=$FSTYPE" >&2; exit 75; }
[[ -z "$EXPECTED_SOURCE" || "$SOURCE" == "$EXPECTED_SOURCE" ]] || { echo "storage profile source mismatch: expected=$EXPECTED_SOURCE actual=$SOURCE" >&2; exit 75; }

while IFS= read -r required_opt; do
  [[ -n "$required_opt" ]] || continue
  case ",$OPTIONS," in
    *,"$required_opt",*) ;;
    *) echo "storage profile mount option missing: $required_opt (actual=$OPTIONS)" >&2; exit 75 ;;
  esac
done < <(jq -r '.expected.mount_options[]? // empty' "$PROFILE")

if jq -e '.expected.zfs? | type == "object"' "$PROFILE" >/dev/null; then
  [[ "$FSTYPE" == zfs ]] || { echo "profile declares expected.zfs but actual filesystem is $FSTYPE" >&2; exit 75; }
  command -v zfs >/dev/null 2>&1 || { echo "required tool not found for ZFS profile: zfs" >&2; exit 127; }
  while IFS=$'\t' read -r prop expected; do
    [[ -n "$prop" ]] || continue
    actual=$(zfs get -H -o value "$prop" "$SOURCE" 2>/dev/null || true)
    [[ "$actual" == "$expected" ]] || { echo "ZFS profile mismatch: $prop expected=$expected actual=$actual dataset=$SOURCE" >&2; exit 75; }
  done < <(jq -r '.expected.zfs | to_entries[] | [.key, (.value|tostring)] | @tsv' "$PROFILE")
fi

PROFILE_SHA=$(sha256sum "$PROFILE" | awk '{print $1}')
export DBBENCH_STORAGE_PROFILE_ID="$PROFILE_ID"
export DBBENCH_STORAGE_PROFILE_MODE="$MODE"
export DBBENCH_STORAGE_PROFILE_SHA256="$PROFILE_SHA"
export DBBENCH_STORAGE_PROFILE_PATH="$PROFILE"
export DBBENCH_STORAGE_ROOT="$ROOT_PATH"

if [[ "$MODE" == native ]]; then
  exec "$@"
fi

command -v systemd-run >/dev/null 2>&1 || { echo "required tool not found for bounded profile: systemd-run" >&2; exit 127; }
if (( EUID != 0 )); then
  echo "bounded storage profiles require root; invoke this wrapper with sudo -E" >&2
  exit 77
fi
jq -e '.limits | type == "object"' "$PROFILE" >/dev/null || { echo "bounded profile has no limits object" >&2; exit 2; }

declare -a PROPS=()
add_prop() { PROPS+=(-p "$1"); }

ALLOWED_CPUS=$(jq -r '.limits.allowed_cpus // empty' "$PROFILE")
CPU_QUOTA=$(jq -r '.limits.cpu_quota_percent // empty' "$PROFILE")
MEMORY_MAX=$(jq -r '.limits.memory_max_bytes // empty' "$PROFILE")
SWAP_MAX=$(jq -r '.limits.memory_swap_max_bytes // empty' "$PROFILE")
[[ -z "$ALLOWED_CPUS" ]] || add_prop "AllowedCPUs=$ALLOWED_CPUS"
[[ -z "$CPU_QUOTA" ]] || add_prop "CPUQuota=${CPU_QUOTA}%"
[[ -z "$MEMORY_MAX" ]] || add_prop "MemoryMax=$MEMORY_MAX"
[[ -z "$SWAP_MAX" ]] || add_prop "MemorySwapMax=$SWAP_MAX"

IO_DEVICE=$(jq -r '.limits.io.device // empty' "$PROFILE")
READ_BPS=$(jq -r '.limits.io.read_bps // empty' "$PROFILE")
WRITE_BPS=$(jq -r '.limits.io.write_bps // empty' "$PROFILE")
READ_IOPS=$(jq -r '.limits.io.read_iops // empty' "$PROFILE")
WRITE_IOPS=$(jq -r '.limits.io.write_iops // empty' "$PROFILE")
if [[ -n "$READ_BPS$WRITE_BPS$READ_IOPS$WRITE_IOPS" ]]; then
  [[ -n "$IO_DEVICE" && -b "$IO_DEVICE" ]] || { echo "bounded profile I/O limits require a block-device limits.io.device: $IO_DEVICE" >&2; exit 2; }
fi
[[ -z "$READ_BPS" ]] || add_prop "IOReadBandwidthMax=$IO_DEVICE $READ_BPS"
[[ -z "$WRITE_BPS" ]] || add_prop "IOWriteBandwidthMax=$IO_DEVICE $WRITE_BPS"
[[ -z "$READ_IOPS" ]] || add_prop "IOReadIOPSMax=$IO_DEVICE $READ_IOPS"
[[ -z "$WRITE_IOPS" ]] || add_prop "IOWriteIOPSMax=$IO_DEVICE $WRITE_IOPS"

UNIT_SAFE=$(printf '%s' "$PROFILE_ID" | tr -c 'A-Za-z0-9_.-' '-')
UNIT="dbbench-storage-${UNIT_SAFE}-$$"

declare -a IDENTITY_ARGS=()
if [[ -n "${SUDO_UID:-}" && -n "${SUDO_GID:-}" && "${SUDO_UID}" != 0 ]]; then
  IDENTITY_ARGS+=(--uid="$SUDO_UID" --gid="$SUDO_GID")
fi

exec systemd-run --quiet --wait --collect --pipe --unit="$UNIT" \
  "${IDENTITY_ARGS[@]}" \
  --setenv="DBBENCH_STORAGE_PROFILE_ID=$DBBENCH_STORAGE_PROFILE_ID" \
  --setenv="DBBENCH_STORAGE_PROFILE_MODE=$DBBENCH_STORAGE_PROFILE_MODE" \
  --setenv="DBBENCH_STORAGE_PROFILE_SHA256=$DBBENCH_STORAGE_PROFILE_SHA256" \
  --setenv="DBBENCH_STORAGE_PROFILE_PATH=$DBBENCH_STORAGE_PROFILE_PATH" \
  --setenv="DBBENCH_STORAGE_ROOT=$DBBENCH_STORAGE_ROOT" \
  "${PROPS[@]}" -- "$@"
