#!/usr/bin/env bash
# diff.sh <baseline-dir> <candidate-dir>
# Per-capture PASS/FAIL + exit_code comparison; writes
# <candidate-dir>/_diff-report/matrix.json (feature_id -> scenario -> PASS/FAIL).
# No tmux dependency - safe to run inside the sandbox once run.sh output exists.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PYTHON3="$(command -v python3 || true)"

if [ -z "$PYTHON3" ]; then
  echo "error: python3 not found in PATH" >&2
  exit 1
fi
if [ "$#" -ne 2 ]; then
  echo "usage: $0 <baseline-dir> <candidate-dir>" >&2
  exit 2
fi

exec "$PYTHON3" "$HERE/diff.py" "$@"
