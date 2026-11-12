#!/usr/bin/env bash
set -euo pipefail
# An orchestration-assigned identity overrides image-global /etc/machine-id.
if [[ -n "${DBBENCH_MACHINE_ID_SHA256:-}" ]]; then
  if [[ ! "$DBBENCH_MACHINE_ID_SHA256" =~ ^[[:xdigit:]]{64}$ ]]; then
    echo "DBBENCH_MACHINE_ID_SHA256 must contain 64 hexadecimal characters" >&2
    exit 2
  fi
  printf '%s\n' "${DBBENCH_MACHINE_ID_SHA256,,}"
elif [[ -s /etc/machine-id ]]; then
  sha256sum /etc/machine-id | awk '{print $1}'
else
  echo "missing machine identity (set DBBENCH_MACHINE_ID_SHA256 on rentals)" >&2
  exit 2
fi
