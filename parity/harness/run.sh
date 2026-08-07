#!/usr/bin/env bash
# Runner IF per CONTRACT.md: run.sh <binary> <scenarios-dir|scenario.json> <outdir>
#
# Drives tmux directly (dedicated socket "$TMPDIR/rust-less-parity.sock") -
# MUST be run outside the agent sandbox (Unix-socket restricted there). This
# is a thin wrapper; all interpretation logic lives in run.py.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PYTHON3="$(command -v python3 || true)"

if [ -z "$PYTHON3" ]; then
  echo "error: python3 not found in PATH" >&2
  exit 1
fi
if ! command -v tmux >/dev/null 2>&1; then
  echo "error: tmux not found in PATH" >&2
  exit 1
fi
if [ "$#" -ne 3 ]; then
  echo "usage: $0 <binary> <scenarios-dir|scenario.json> <outdir>" >&2
  exit 2
fi

exec "$PYTHON3" "$HERE/run.py" "$@"
