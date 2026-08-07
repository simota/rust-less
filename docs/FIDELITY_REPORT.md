# Fidelity Report — rust-less v0.1.0 (binary: `rless`)

Clean-room Rust reimplementation of the core pager behavior of GNU less 668,
verified by **differential parity against a captured baseline** (not by assertion).
Run: `/nexus clone`, 2026-08-08.

## Verdict

**FAITHFUL-COPY** (independent fidelity review) · **parity 40/40 scenarios green** ·
drift status **no-drift**.

## Rights Record

- Authorization basis: black-box behavioral reproduction of a published CLI tool
  (user-declared). GNU less source code was never read at any phase (clean-room;
  verified by independent review — behavioral comments cite capture names, never
  less internals).
- Capture posture: local binary driven in a local pty — ToS/robots/rate limits N/A.
- PII: synthetic fixtures only. Assets: terminal text only; the `h` help screen
  (verbatim upstream text) is out of scope by design.
- License of this work: MIT OR Apache-2.0.

## Stack Decision Record (locked before capture)

Rust 1.92 (edition 2024) / crossterm 0.29 (`use-dev-tty`) / clap 4 / regex 1 /
unicode-width 0.2. Declared parity ceilings: (1) regex-crate vs POSIX regex
dialect divergence (backreferences etc.) — declared tolerance, common ERE
patterns verified; (2) terminal init sequence differences — canonicalized away
by comparing screen *states*, not escape streams.

## Provenance Stamp & Drift

- Target: GNU less 668 (POSIX regular expressions build), `/usr/bin/less`
- Environment: macOS Darwin 25.5.0 arm64, tmux 3.7b pane 80x24 (fixed),
  LANG=C.UTF-8, env scrub `LESS`/`LESSOPEN`/`LESSCLOSE` unset + `LESSHISTFILE=-`
- Capture date: 2026-08-08 · fixture SHA256s in `parity/provenance.json`
- **Drift re-check (pre-ship): full 40-scenario recapture of `/usr/bin/less`
  diffed against the stamped baseline — identical (no-drift).** Version-drift
  signal (T2): upstream is at less-704; the baseline deliberately pins the
  locally shipped 668.

## Capture Mechanism & Adapter Conformance

tmux-based adapter (fixed-size pane, `send-keys`, `capture-pane [-e]`), emitting
all four conformant-adapter artifacts: state screenshots (`.txt`/`.attrs.txt`),
interaction log (`steps.log`), state graph (scenario set × feature_ids),
provenance (`parity/provenance.json`). Baseline determinism proven by a full
self-consistency recapture (39/39, later 40/40 — zero unmasked non-determinism).

## Research Grounding

Path: repo-native (local man page = version-exact T1; one WebFetch to
greenwoodsoftware.com for drift signal). Evidence Ledger: 48 entries
(T1 47 / T2 1) — final verification state: 36 confirmed-by-capture,
11 unconfirmed-lead, 1 capture-determined (resize repaint *mechanism*).
0 contradicted-by-capture. Notable capture-corrected interpretation: malformed
UTF-8 bytes render per-byte via LESSBINFMT (`<E4><B8>`), not LESSUTFBINFMT.

## Capture Coverage

- Declared inventory: **35 features** — 35/35 covered by ≥1 scenario
  (completeness gate green, no silent omission).
- 40 scenarios · 107 state captures (+ attrs variants) · 40 exit codes.
- Canonicalization contract: fixed 80x24 (resize scenario: →100x30; odd-height
  scenario: 80x25), env scrub, deterministic synthetic fixtures — no masking
  required; `.attrs.txt` compared only where attributes are the spec
  (`compare_attrs: true`).

## Per-Dimension Parity Results

| Dimension | Comparator | Result |
|-----------|-----------|--------|
| Visual + Behavioral | cell-grid exact match per capture (canonicalized) + exit codes | **40/40 scenarios, 0 diffs** (converged in 3 cycles: 28→39→40) |
| Feature | coverage matrix vs declared inventory | **35/35** |
| Data/API · Asset · Performance | — | N/A (not declared for this run) |

Measured-fact highlights the clone reproduces exactly: fresh-launch
bottom-anchor for short files (vs top-anchor+tildes after `:n`/`:p`),
`half_window = ceil(rows/2)` + sticky `Nd`/`Nu`, non-clamping `Ng`
("Cannot seek to line number N  (press RETURN)") vs clamping relative scroll,
left-edge prompt truncation, binary-file confirmation prompt (+ exit 1 on
decline), search highlight SGR pair `38;5;235`/`48;5;214` with `39`/`49` reset,
`-R` per-line color reset and almost-ANSI swallowing,
"Pattern not found  (press RETURN)" (two spaces).

## Coverage Gaps (named, honest — the `feature`-backlog handoff)

- **Deferred features (12, by user scope decision):** mark (m/'), follow (F),
  filter (&), -i, -I, +cmd, help screen (h), editor (v), horizontal scroll
  (ESC-(/ESC-)), -p, -F, -X.
- **UNVERIFIED (implemented from man-page claims, no baseline oracle):**
  bell audibility; resize repaint mechanism/timing (end-state IS verified);
  ESC-v/ESC-</ESC-> aliases; -m/-M byte-fallback branch; LESSBINFMT hex
  fallback edge; wrap-line × window-boundary scroll; `:n`/`:p` to nonexistent
  file; -K/^C exit 2; full list in `docs/RECONSTRUCTION_SPEC.md` §5.

## Regression Harness (handoff contract)

`parity/` is a re-runnable differential suite:
`bash parity/harness/run.sh <binary> parity/scenarios <outdir>` then
`bash parity/harness/diff.sh parity/baseline <outdir>`. Any later change to
rless must re-run it; a parity break is either a recorded deliberate
divergence or a regression. tmux requires sandbox-exempt execution
(Unix socket creation).

## Build & Tests

`cargo build --release`: 0 warnings. `cargo test`: 54/54. Source: 2,636 lines
across cli/buffer/screen/input/search/prompt/command modules (idiomatic-Rust
verdict from independent review; zero `unsafe`).
