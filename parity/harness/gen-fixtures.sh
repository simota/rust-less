#!/usr/bin/env bash
# Generate deterministic synthetic fixtures into parity/fixtures/.
# See parity/harness/gen_fixtures.py for content; see CONTRACT.md for the
# fixture contract (synthetic-only, no timestamps/PII/randomness).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PYTHON3="$(command -v python3)"

if [ -z "$PYTHON3" ]; then
  echo "error: python3 not found in PATH" >&2
  exit 1
fi

exec "$PYTHON3" "$HERE/gen_fixtures.py"
