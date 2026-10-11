#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
[[ ${EUID:-$(id -u)} -eq 0 ]] || { echo "must run as root" >&2; exit 77; }
[[ -x /usr/local/libexec/sccache-bin/sccache ]] || { echo "missing /usr/local/libexec/sccache-bin/sccache" >&2; exit 2; }
[[ -s /etc/sccache/garage-laptop.env ]] || { echo "missing /etc/sccache/garage-laptop.env" >&2; exit 2; }
install -d -m 0755 /etc/sccache /etc/systemd/system
install -m 0644 "$ROOT/scripts/sccache-native-local.conf" /etc/sccache/client-native-local.conf
install -m 0644 "$ROOT/scripts/sccache-native-root.service" /etc/systemd/system/sccache-native-root.service
# The canonical unit now contains the persistence setting directly; discard the
# earlier emergency drop-in if present so effective state is reproducible.
rm -f /etc/systemd/system/sccache-native-root.service.d/20-persistent.conf
systemctl daemon-reload
systemd-analyze verify /etc/systemd/system/sccache-native-root.service
if [[ ${1:-} == --restart ]]; then
  systemctl reset-failed sccache-native-root.service || true
  systemctl restart sccache-native-root.service
fi
