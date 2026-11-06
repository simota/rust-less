# FVI — Feature Value Index: rust-less (`rless`)

Evaluation date: 2026-11-06. Baseline commit: `d541fa0`.
§1–§3 (product model, task set, rubric) and §4 (baseline) were written
**before any code change** and are not edited afterwards. §5 onwards is
appended when the work is done.

## 1. Product model (from the code, not from user data)

`rless` is a terminal pager with two layers:

- **Core pager**: a clean-room clone of GNU less 668 (navigation, search,
  `-N -S -R -m -M -q -Q`, multi-file). Its screen output is a contract,
  checked byte for byte by `make parity` (40 scenarios). Any new feature must
  keep 40/40 and must not change the screen of any key sequence the
  scenarios use.
- **Structured-log layer**: format sniffing (JSON lines, CSV, TSV, logfmt),
  table / expanded views, column mode, `&` filter, selector queries.

No usage data or user requests exist in the repository. The user model below
is therefore a **hypothesis** inferred from the code and its docs: developers
and operators reading application logs, CSV exports and command output,
locally or piped (`kubectl logs | rless`, `git log | rless`).

Main jobs (hypothesis): (J1) read and navigate a file or stream, (J2) find
things, (J3) narrow a log down to what matters, (J4) watch a log as it grows,
(J5) analyse structured data (which values, how often, the worst ones),
(J6) use rless as a drop-in `less` (scripts, `git`, `LESS=...`),
(J7) keep or share what was found.

## 2. Task set (fixed before changes)

One task per user goal; a task counts as **achievable** only if the documented
keys or flags complete it end to end with a correct screen.

| ID | Job | Task |
|---|---|---|
| T01 | J1 | View a file or stdin and page through it |
| T02 | J2 | Regex search forward/backward, repeat with n/N |
| T03 | J2 | Case-insensitive search (`-i` / `-I`) |
| T04 | J3 | Show only the lines of a plain-text log matching a regex (`&ERROR`) |
| T05 | J3 | Hide lines matching a regex (`&!DEBUG`) |
| T06 | J3 | Navigate a filtered view (j/k/G/search) without landing on hidden lines |
| T07 | J3 | Filter a structured log with one predicate (`&.status >= 500`) |
| T08 | J3 | Combine predicates (`&.level == "ERROR" && .service == "auth"`, `||`, `!`) |
| T09 | J3 | Filter on a nested JSON field (`&.user.id == 7`) |
| T10 | J3 | Regex match on a field value (`&.msg =~ "timeout|refused"`) |
| T11 | J4 | Follow a growing file (`F`, `+F`) |
| T12 | J4 | Follow a piped stream (`cmd | rless`, then `F`) |
| T13 | J5 | Sort the table by a column, ascending/descending |
| T14 | J5 | See the value distribution / numeric summary of a column |
| T15 | J1 | Scroll horizontally in a chopped (`-S`) view or a wide table |
| T16 | J6 | Start at the end, a line or a pattern (`+G`, `+50`, `-p pat`, `+/pat`) |
| T17 | J6 | Work as `git`'s pager: `-F` (quit if one screen), `-X`, `LESS` env, output to a non-terminal |
| T18 | J1 | Toggle line numbers / chopping / case folding without restarting (`-N`, `-S`, `-i` typed in the pager) |
| T19 | J1 | Mark a position and come back to it (`m a`, `' a`, `''`) |
| T20 | J1 | Find the keys from inside the pager (`h`) |
| T21 | J7 | Save the current (filtered / sorted) view to a file |
| T22 | J5 | Table view, pin/hide columns, expand a JSON line |

## 3. Rubric (fixed)

FVI = 0.25·UVI + 0.25·FCI + 0.20·CEI + 0.15·UEI + 0.15·FRI, each 0–100.

| Index | How it is scored |
|---|---|
| UVI user value | Share of the 7 jobs (J1–J7) for which every task of that job is achievable: 100 × jobs fully served / 7 |
| FCI completeness | 100 × achievable tasks / 22 (a partly working task counts 0.5) |
| CEI capability | 100 × (distinct task capabilities available) / 22 — counts *what can be done*, not how many keys exist; identical to FCI in the baseline by construction, it diverges only if a task is complete but reachable only one way |
| UEI efficiency | Mean keystrokes, over the tasks achievable both before and after, to complete them, mapped as 100 × (1 − extra steps / steps before), plus a discoverability check: `h` help (+0/+10), prompt shows active view state (+0/+10). Capped at 100; baseline = 80 |
| FRI reliability | Starts at 100; −10 per known defect that makes a shipped feature show something false or do nothing silently (the SQI report's residual list), −20 if parity < 40/40, −20 if any test fails |

Scores are rubric scores (E1/E2 evidence), not user measurements.

## 4. Baseline (before), commit d541fa0

Measured: `cargo test` 102 passed / 0 failed / 9 ignored; clippy
`-D warnings` clean; `make parity` 40/40.

Achievable tasks: T01, T02, T07, T22 = 4. T06 partly (j/k may land on a
hidden line; G/search ignore the filter) = 0.5. All others: not provided.
T04/T05: `&` on plain text reports "no effect". T13: `s` in column mode
reports "not supported yet". T11/T12: no follow mode; a stdin read blocks the
UI while the producer is idle. T17: rless writes escape sequences into a
pipe when stdout is not a terminal.

| Index | Baseline | Basis |
|---|---:|---|
| UVI | 0.0 | No job fully served (J1 lacks T15/T18/T19/T20, J2 lacks T03, …) |
| FCI | 20.5 | 4.5 / 22 |
| CEI | 20.5 | 4.5 / 22 |
| UEI | 80 | baseline anchor; no help screen, no view state in the prompt |
| FRI | 70 | Known defects: j/k into hidden lines; filter kept across files to a file of another format; sort stub (3 × −10) |
| **FVI** | **37.5** | 0.25·0 + 0.25·20.5 + 0.2·20.5 + 0.15·80 + 0.15·70 |

## 5. After, at HEAD of `claude/rust-less-feature-expansion-rr76w5`

### 5.1 Measurements

| Instrument | Before (`d541fa0`) | After |
|---|---|---|
| `cargo test` | 102 passed, 0 failed, 9 ignored | **136 passed**, 0 failed, 9 ignored |
| `cargo fmt --check`, `clippy --all-targets -D warnings` | clean | clean |
| `make parity` (40 less scenarios) | 40/40 | **40/40** |
| `make validate` | 35/35 features covered | 35/35 |
| `docs/fvi/fvi_probe.py` (22 tasks, §2) | **4/22** | **22/22** |
| `docs/sqi/ux_probe.py` (previous 17 tasks) | 17/17 | 17/17 |

Both probe columns come from the same script run against both binaries
(the baseline built from `d541fa0` in a worktree). Three success conditions
of the new probe were corrected after their first run, before the baseline
run, because they checked the wrong screen (a value clipped off a table row,
too few Right presses for the fixture, a help line on the second page); the
product was not changed for them.

### 5.2 Scores

| Index | Before | After | Basis (after) |
|---|---:|---:|---|
| UVI | 0.0 | 100.0 | All 7 jobs have every task achievable (22/22) |
| FCI | 20.5 | 100.0 | 22 / 22 |
| CEI | 20.5 | 100.0 | 22 / 22 task capabilities available |
| UEI | 80 | 100 | Keystrokes for T01/T02/T07/T22 unchanged (80 anchor); `h` help (+10); view state in the prompt (+10) |
| FRI | 70 | 100 | The 3 baseline defects are gone (hidden-line navigation fixed; the filter is kept across files by design, as in less, and shown in the prompt; sort implemented). One found during the work (`-M` in a sorted view mixed buffer lines and rows) was fixed. No known defect remains; this is not a claim that none exists |
| **FVI** | **37.5** | **100.0** | |

FVI is a rubric score over a task set chosen by the implementer from a
hypothesised user model (§1). It saturates because every chosen task now
passes; it says nothing about tasks outside the set or about real users.

### 5.3 What users can now do (new capabilities)

- Filter any file, plain or structured (`&pat`, `&!pat`), and move, search
  and jump inside the filtered view without ever landing on a hidden line.
- Combine selector predicates (`&&`, `||`, `!`, parentheses), address nested
  JSON (`.user.id`), test existence (`.field`, `!.field`), regex-match values
  (`=~`), compare timestamps as text and `1.5s`-style values as durations,
  and use all of it on CSV/TSV columns by header name (before: one predicate,
  top-level keys only, never matching CSV/TSV).
- Follow a growing file or a pipe (`F`, `+F`), with a filter or sort kept
  live, unfinished lines joined, and truncation/rotation handled. A slow or
  idle pipe no longer blocks the first screen or `G`.
- Sort the table by any column (numeric-aware, missing values last) and open
  a statistics page per column (counts, distinct values, top values,
  min/p50/p90/p99/max/mean).
- Scroll sideways under `-S` and in wide tables; column focus brings the
  focused column into view.
- Set marks and return (`m`, `'`, `''`); see every key in the pager (`h`);
  toggle `-N -S -i -I` without restarting; save the current view (`:w`).
- Start positioned (`+G`, `+120`, `-p pat`, `+/pat`, `+&pat`), fold case
  (`-i` smart, `-I` always), quit when the input fits (`-F`), keep the last
  page on screen (`-X`), pick up flags from `$LESS`, and act like `cat` when
  stdout is not a terminal, which makes rless usable as git's pager.

### 5.4 Performance (release, same machine, interleaved runs)

| Bench | Before p50 | After p50 | Note |
|---|---:|---:|---|
| filter_regex_draw_200k | 766 µs | 14 µs | the view index caches matches; a redraw no longer rescans |
| draw_scroll_plain_x500 | 40.8–40.9 ms | 36.4–36.7 ms | within noise / slightly faster |
| render_window_plain_x1000 | 22.7 ms | 21.5–24.4 ms | noise |
| search_forward_regex_1m | 36.9–39.2 ms | 39.9–41.0 ms | within the ±9 % build-to-build band the SQI report measured |
| search_backward_near_top_1m | 95.8–108.9 ms | 97.0–101.0 ms | noise |
| buffer_full_read_1m | 95.8–105.7 ms | 98.4–110.2 ms | at most +2–4 %, inside noise; one extra branch per line for follow mode's unfinished-line hold |
| structured_search_200k | 31.9–39.9 ms | 29.5–33.6 ms | noise |
| tabular_draw_x200 | 12.4–17.5 ms | 11.7–12.0 ms | noise |

Peak RSS is unchanged (≈118 MB, dominated by the 1M-line fixtures). New
costs that exist only when a feature is used: a sorted view holds one key and
one position per line; follow mode polls every 200 ms; stdin is read by one
background thread through a channel bounded to 8 × 64 KiB.

### 5.5 Residual risks, limits and items not verified

- **No real-user testing.** All task evidence is scripted (E2).
- The new keys and flags have **no less oracle**: they are specified by unit
  tests and the probe, not by `make parity` (docs/RECONSTRUCTION_SPEC.md,
  "Extensions after Phase 5", lists the known differences from less).
- A sorted view is rebuilt whenever follow mode appends lines: O(n log n) per
  poll on very large files.
- On a live pipe, a filter that needs more lines than have arrived waits up
  to 150 ms (the idle window) per redraw.
- Rotation detection uses (device, inode) and is Unix-only; elsewhere only
  truncation is detected.
- `-F` prints the rendered input instead of drawing it on the screen; with
  `-R` this is the raw coloured text, which is what git expects.
- Not verified: terminals other than tmux, very slow filesystems, Windows.
