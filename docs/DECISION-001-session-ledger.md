# DECISION-001 — Session Ledger: decided NOT to build

**Date:** 2026-08-08 · **Verdict:** NO-GO (confidence High) · **Status:** closed, re-evaluable

A `killer` run asked one question: what single feature would make `rless` worth
using over `less`? The funnel converged on **Session Ledger**, and the gate
rejected it. This record exists so the rejection is auditable and so a later
re-evaluation starts from the evidence rather than from scratch.

## The candidate

`rless --record out.rlog` — page normally while incoming bytes, arrival times,
key actions and terminal size land in an append-only ledger; the screen becomes
a *projection* of that ledger. `--replay` restores exit position, search term
and scroll position and scrubs the key-action timeline. `--diff a.rlog b.rlog`
cell-grid-diffs two runs of the same command. Content-addressed, off by default.

Claimed moat: other pagers are structurally read-and-discard, so this cannot be
bolted on; rless alone already operates a cell-grid diff engine (40/40) that
proves a replayer faithful.

## Why it was rejected

1. **The core is self-contradictory.** "The screen is a projection of the
   ledger" and "off by default preserves 40/40 parity" cannot both hold.
   `src/buffer.rs:69-92` is a demand-driven lazy pull, so what is recordable is
   the time rless *decided to read*, not arrival time. Making arrival time real
   requires an eager reader, which moves the very paths the 40 scenarios pin
   (clamp `src/command.rs:324-342`, EOF peek `src/screen.rs:388-394`, `-M`
   prompt `src/command.rs:522-530`).
2. **The cheap-first-release case rested on three factual errors.**
   `parity/harness/diff.py:31-33` holds no comparison rules — it is a raw byte
   compare; every normalization that makes it meaningful lives in the
   environment (fixed 80x24 tmux pane, env scrub, settle wait). So `--diff`
   ports *tmux's terminal emulator*, not "diff.py's rules". Recording also
   touches 5 of 6 modules, not "behind buffer.rs". And `--replay` cannot restore
   a screen from a position: `first_render_done`, `filename_shown`,
   `sticky_half` and `pending_message` are path-dependent, so faithful replay
   means re-executing every action from process start. Estimated increment
   1,500-1,900 lines against 2,916 existing — most of it reimplementing tmux,
   not less. (Both `diff.py` and the module spread were independently confirmed
   at hub level.)
3. **The secrets deadlock is a logical bind, not a measurement gap.** Off by
   default means the run that surprised you was never recorded; on by default
   makes the pager a new persistent copy of terminal output. Two skeptics on
   different engines reached this independently (Kubernetes secret-handling
   guidance; RHEL session recording disables input capture by default).
   Redaction cannot coexist with "the screen is a projection" — redact and the
   projection no longer matches the original screen.
4. **The moat is a cost, not an asset.** The only thing slowing imitation is the
   cell-grid diff engine — which is exactly our own most expensive component.

## What survived refutation (the real remaining whitespace)

- **No tool cell-grid-diffs two interactive sessions.** The market skeptic
  searched and could not refute this.
- **No recorder verifies its own replay fidelity.** rless's parity harness could
  — but today that is a *development asset*, not a user-facing feature.
- Recording piped stdin is cheap (`ensure_full_read` already buffers).
- Resize replay is verifiable against the existing oracle if limited to
  end-state.

## What collapsed (do not re-derive these)

| Claim | Refuted by |
|---|---|
| Recording model is novel | asciicast v3 `o`/`i`/`r` events; util-linux `script` I/O/H/S streams |
| Position + search restore is unserved | `lnav` sessions (auto save/restore, `:export-session-to`) |
| Screen-state time scrubbing is unserved | iTerm2 Instant Replay (terminal layer) |
| Other pagers are structurally read-and-discard | `less -o/--log-file` (verified in the local man page); `ov` SaveBuffer |
| Cell-grid diff has no analogue | partially overlaps VHS `.txt`/`.ascii` golden files |

**Recorded nuance:** the market refutation overreached in places — `less -o`
copies piped input bytes only, with no key actions, terminal size, replay or
diff. The decisive refutations were the feasibility and self-consistency ones,
not the market one.

## Rejected alternatives (from the same funnel)

- Unified structured-data pager (CSV/JSON/log) — scope explosion; incumbents are
  each deep in one format.
- Latency-tolerant redraw engine for SSH/tmux — real gap, but an optimization of
  the read-and-discard model that leaves no compounding asset.
- Search match counts / structural breadcrumbs — genuinely wanted, but
  copyable within one release: table stakes, not a differentiator.

## Runner-up, with its weakest link

**`--diff`-only mode** (drop ledger/record/replay; compare two runs of one
command). Dissolves the secrets deadlock, needs no eager reader, touches no
parity path, and occupies the surviving whitespace directly.

Weakest link: the terminal-emulator debt remains, and the dominant case is
served by `diff <(cmd) <(cmd)` — the advantage narrows to TUI redraw / ANSI /
wrapping artifacts, which overlaps VHS golden files.

**Verify before building:** is there real, frequent demand in that narrow band
to justify implementing a terminal emulator? If no, the runner-up should be
rejected too.

## Method

Compete (market, web-sourced) ‖ Plea (latent needs) ‖ Flux (tech reframe,
claude) ‖ Flux (tech reframe, **codex** — separate model priors) → Spark
(single-feature synthesis) → moat analysis + 3-skeptic adversarial panel
(market / demand-codex / feasibility-with-code-reading) → Magi binary verdict.
Panel split: 1 refuted-on-evidence, 2 survived-but-unproven; Magi declined the
mechanical majority because the "survived" labels covered reports whose contents
refuted every load-bearing premise.
