#!/usr/bin/env bash
# Generate parity/scenarios/*.json from parity/harness/gen_scenarios.py.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PYTHON3="$(command -v python3 || true)"

if [ -z "$PYTHON3" ]; then
  echo "error: python3 not found in PATH" >&2
  exit 1
fi

exec "$PYTHON3" "$HERE/gen_scenarios.py"
