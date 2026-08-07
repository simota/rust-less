#!/usr/bin/env bash
# Static schema + declared-inventory coverage check for parity/scenarios/*.json.
# No tmux dependency - safe to run inside the sandbox.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PYTHON3="$(command -v python3 || true)"

if [ -z "$PYTHON3" ]; then
  echo "error: python3 not found in PATH" >&2
  exit 1
fi

exec "$PYTHON3" "$HERE/validate_scenarios.py"
