#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
DEST=${TSDB_DEPS_DIR:-"$ROOT/.deps/tsdb"}
mkdir -p "$DEST"

require_tool() {
  command -v "$1" >/dev/null 2>&1 || { echo "missing required tool: $1" >&2; exit 2; }
}
for tool in curl sha256sum tar find install uname; do require_tool "$tool"; done
[[ "$(uname -m)" == "x86_64" ]] || { echo "TSDB binary installer currently supports x86_64 only" >&2; exit 2; }

install_tarball() {
  local name=$1 version=$2 url=$3 sha256=$4 binary_name=$5
  local dir="$DEST/$name-$version" target="$DEST/$name-$version/$binary_name" marker="$DEST/$name-$version/.verified-sha256"
  if [[ -x "$target" && -s "$marker" ]]; then
    local recorded_archive recorded_binary current_binary
    recorded_archive=$(awk -F= '$1 == "archive" {print $2}' "$marker")
    recorded_binary=$(awk -F= '$1 == "binary" {print $2}' "$marker")
    current_binary=$(sha256sum "$target" | awk '{print $1}')
    if [[ "$recorded_archive" == "$sha256" && -n "$recorded_binary" && "$current_binary" == "$recorded_binary" ]]; then
      printf '%s\n' "$target"
      return 0
    fi
    echo "cached $name $version failed provenance verification; reinstalling" >&2
  fi
  rm -rf -- "$dir"
  local tmp
  tmp=$(mktemp -d "$DEST/.${name}-${version}.XXXXXX")
  trap 'rm -rf -- "$tmp"' RETURN
  local archive="$tmp/archive.tar.gz"
  curl -LfsS --retry 3 --retry-delay 1 "$url" -o "$archive"
  printf '%s  %s\n' "$sha256" "$archive" | sha256sum -c - >/dev/null
  mkdir -p "$tmp/extract"
  tar -xzf "$archive" -C "$tmp/extract"
  local found
  found=$(find "$tmp/extract" -type f -name "$binary_name" -print -quit)
  [[ -n "$found" ]] || { echo "archive for $name $version does not contain $binary_name" >&2; exit 2; }
  mkdir -p "$dir"
  install -m 0755 "$found" "$target"
  local binary_sha
  binary_sha=$(sha256sum "$target" | awk '{print $1}')
  printf 'archive=%s\nbinary=%s\n' "$sha256" "$binary_sha" > "$marker"
  printf '%s\n' "$target"
  rm -rf -- "$tmp"
  trap - RETURN
}

GREPTIME_VERSION=1.2.1
VICTORIA_VERSION=1.153.0
PROMETHEUS_VERSION=3.14.0
INFLUXDB3_VERSION=3.12.0

install_tarball greptimedb "$GREPTIME_VERSION" \
  "https://github.com/GreptimeTeam/greptimedb/releases/download/v${GREPTIME_VERSION}/greptime-linux-amd64-v${GREPTIME_VERSION}.tar.gz" \
  "062d5beb13de4991c4fec201b2e5071518bf4ca8f0d52947eba757a74c2a5420" \
  greptime
install_tarball victoriametrics "$VICTORIA_VERSION" \
  "https://github.com/VictoriaMetrics/VictoriaMetrics/releases/download/v${VICTORIA_VERSION}/victoria-metrics-linux-amd64-v${VICTORIA_VERSION}.tar.gz" \
  "1b495bde563825cf83dc7c0425a9d8fa03e7214858e0949f9177efc6bb1f8bfc" \
  victoria-metrics-prod
install_tarball prometheus "$PROMETHEUS_VERSION" \
  "https://github.com/prometheus/prometheus/releases/download/v${PROMETHEUS_VERSION}/prometheus-${PROMETHEUS_VERSION}.linux-amd64.tar.gz" \
  "f665c6da19eb7ba399c915d30c7d9793c9b417bf8a749b504bc470678631478d" \
  prometheus
install_tarball influxdb3 "$INFLUXDB3_VERSION" \
  "https://download.influxdata.com/influxdb/releases/influxdb3-core-${INFLUXDB3_VERSION}_linux_amd64.tar.gz" \
  "bd50a59aa8c7665fa837d877baffdee632bdc321bfca90d5e9706062f9e69645" \
  influxdb3
