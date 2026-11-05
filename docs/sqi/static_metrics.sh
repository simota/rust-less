#!/usr/bin/env bash
# SQI static metrics (docs/sqi/SQI_REPORT.md). Grep-based counts over src/,
# test modules included unless stated. Same script before and after.
set -u
cd "$(dirname "$0")/../.."
nontest() { # strip `#[cfg(test)] mod tests { ... }` (always the file tail here)
  awk '/^#\[cfg\(test\)\]$/{getline n; if (n ~ /^mod tests/) exit; print; print n; next} {print}' "$1"
}
echo "per-format scanner dispatch sites (scan_*_line calls outside simd.rs):"
grep -c "StructuralClassifier::scan_" src/projection.rs src/selector.rs src/command.rs 2>/dev/null | awk -F: '{s+=$2; print "  "$0} END {print "  total="s}'
echo "allow(dead_code) attributes:      $(grep -c 'allow(dead_code)' src/*.rs | awk -F: '{s+=$2} END {print s}')"
echo "pub fields on App:                $(nontest src/command.rs | awk '/^pub struct App/,/^}/' | grep -c '^    pub ')"
echo "fields never read (projection):   $(for f in sorted_by_column projection_index; do grep -c "$f" src/*.rs | awk -F: '{s+=$2} END {print s}'; done | tr '\n' ' ')(occurrences; assignment-only = dead)"
echo "duplicate prompt-mode enums:      $(grep -c 'pub enum PromptMode\|pub enum Mode' src/cli.rs src/prompt.rs | awk -F: '{s+=$2} END {print s}')"
echo "lines in App::draw_frame:         $(awk '/fn draw_frame/{f=1} f{n++} f&&/^    }$/{print n; exit}' src/command.rs)"
echo "control-char sanitizers on projection output: $(grep -c 'caret\|sanitize' src/projection.rs)"
echo "non-test unwrap()/expect( in src: $(for f in src/*.rs; do [ "$f" = src/bench.rs ] && continue; nontest "$f" | grep -c 'unwrap()\|expect('; done | awk '{s+=$1} END {print s}')"
echo "unit tests (#[test]):             $(grep -c '#\[test\]' src/*.rs | awk -F: '{s+=$2} END {print s}')"
echo "source lines (src/*.rs):          $(cat src/*.rs | wc -l)"
