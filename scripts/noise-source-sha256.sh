#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
cd -- "$1"
sha256sum -- tools/benchctl/src/noise.rs tools/benchctl/src/noise/*.rs | sha256sum | cut -d ' ' -f1
