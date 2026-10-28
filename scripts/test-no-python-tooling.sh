#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

if [[ -n "$(git ls-files '*.py')" ]]; then
  echo "tracked Python files are not allowed in the benchmark control plane" >&2
  git ls-files '*.py' >&2
  exit 1
fi

if grep -RInE --exclude=test-no-python-tooling.sh 'uv run|(^|[[:space:]])python3?([[:space:]]|-m)|^#!.*python' scripts; then
  echo "first-party scripts must not invoke Python or uv" >&2
  exit 1
fi

if grep -RInF --exclude=test-no-python-tooling.sh '/srv/scratch/db-bench-2026-09-27' README.md docs scripts; then
  echo "retired scratch checkout path returned to active tooling/docs" >&2
  exit 1
fi

echo no-python-tooling-ok
