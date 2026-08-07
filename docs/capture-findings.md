# Capture Findings — GNU less 668 baseline (parity/baseline/)

Source: `parity/baseline/` (39 scenarios, `/usr/bin/less` on Darwin 25.5.0,
tmux 3.7b, 80x24, `env -u LESS -u LESSOPEN -u LESSCLOSE LESSHISTFILE=- LANG=C.UTF-8`).
Read-only analysis of the baseline dump; no tmux executed, no `src/` touched,
less source not consulted. All row/column numbers below are 1-indexed against
an 80-col × 24-row pane unless stated otherwise.

## The 5 capture-determined items

### (a) Arrow / PageUp / PageDown bindings
`nav-arrow-pgup-pgdn/`, positioned at line 50 first (`50g`):

Sequence tested: start at line 50 (`50g`) → Down → Up → NPage → PPage.

| key | top-of-window before → after | matches |
|---|---|---|
| Down | 050 → 051 | `j`/`^N` (forward 1 line) |
| Up | 051 → 050 | `k`/`^P` (backward 1 line) |
| PageDown (NPage) | 050 → 073 | `f`/`^F`/Space (forward 1 window; window = 23 content rows, 050‑072 visible, next window starts 073) |
| PageUp (PPage) | 073 → 050 | `b` (backward 1 window) |

Confirmed: arrows = line-scroll aliases of j/k, PageUp/PageDown = window-scroll
aliases of b/f, exactly as terminfo-based binding would suggest. No man-page
or `--help` text states this; it was undocumented prior to this capture.

### (b) Search failure message
`search-not-found/after-{forward,backward}-search-fail.txt`, both directions:

```
Pattern not found  (press RETURN)
```

(two spaces before the parenthetical). Position: bottom status line (row 24),
replacing the normal prompt. `.attrs.txt` shows it is rendered with
`ESC[38;5;235mESC[48;5;214m...ESC[39mESC[49m` — a 256-color foreground/background
pair (dark grey on amber), **not** a simple reverse-video (`ESC[7m`) toggle.
This exact color pair also appears on: the initial-filename prompt segment,
search-match highlighting, and the malformed-UTF-8 `<XX>` fallback — see the
environment-contamination caveat below.

### (c) q/Q/:q/:Q/ZZ exit codes
`exit-{lower-q,upper-q,colon-lower-q,colon-upper-q,ZZ}/exit_code`: **all five
forms exit with code 0.** The evidence ledger's "assumed 0 pending capture"
is confirmed for all 5 documented quit forms (not just q).

Bonus data point (not one of the 5, but same underlying claim family):
`exit-code-file-not-found/exit_code` = **1** for a nonexistent file argument.
less exits immediately on file-not-found — so fast that our first capture
step raced it and found the tmux pane already gone (handled gracefully by
run.py's WARN path, confirming that defensive design was necessary in
practice, not just theoretical).

### (d) Prompt reverse video / standout
`prompt-initial-filename/`: the **first** prompt of a file (showing the
filename, e.g. `parity/fixtures/lines200.txt`) is colored with the same
`38;5;235`/`48;5;214` pair as the search-fail message. The **bare `:`
prompt** shown on the second capture (after `j`, mid-file, no filename) has
**zero** attrs — plain text, no color, no standout. So: standout/color is
applied to "special" prompt content (filename, error, (END)-adjacent
messages), not to the plain colon prompt itself.

### (e) Resize redraw (80x24 → 100x30)
`terminal-resize-redraw/`: positioned at line 50 (top=050, 23 content rows
visible) before resize. After `tmux resize-window -x 100 -y 30`:
top-of-window **stays at 050** (not recentered), window grows to 29 content
rows (30 total − 1 prompt row). A subsequent `j` scrolls to 051 normally.
Confirms auto-adjust-on-resize with position preservation. Note: capture-pane
snapshots only show the **end state**, not the escape-sequence transcript of
the redraw itself, so the *mechanism* (full clear vs. incremental) could not
be determined from this capture method — see ledger note below.

## Bonus items requested

### Half-window rounding (d/u), 24-row pane
`nav-halfwindow/`: top 001 → (d) → 013 → (d) → 025 → (u) → 013. Half-window =
**12** = 24/2 exactly (pane height is even, so this capture cannot resolve
the floor/ceil/round ambiguity for an *odd* window height — genuine residual
gap, see "Residual gaps" below). `5d` sets a new sticky default: 013 → 018
(+5), then bare `d` → 023 (+5), bare `u` → 018 (−5): confirms d/u share one
sticky default exactly as documented.

Notable asymmetry: full-window scroll (Space/f) moves by **23** (screen
height − 1, i.e. the content-row count), while half-window scroll (d/u)
moves by **12** = 24/2 (full screen height ÷ 2, not 23÷2). The two "window"
units are not simply related by ÷2.

### :n/:p at file-list boundaries
`startup-multi-file/`: sequential `:n` walks file1→file2→file3, prompt each
time is `<path> (file N of 3) (END) - Next: <next-path>` (file 1/2) or
`<path> (file 3 of 3) (END)` (last, no "Next:" segment) — matches the
documented prompt format string exactly. At the boundary, `:n` on file 3
produces: **`No next file  (press RETURN)`**. Symmetrically, `:p` on file 1
produces: **`No previous file  (press RETURN)`**. Both in the same
235/214 highlight color as the search-fail message. `2:n` / `2:p`
(`multi-file-count-nav/`) correctly jump 2 files at once (file1 → file3 → file1).

## Other capture-determined findings worth flagging for Phase 4

### Fresh-launch short-file rendering is NOT top-anchored+tilde-filled
Every scenario whose **first** capture is the **first** screen of the
**first** file at process startup, when that content (after accounting for
wrapping) is shorter than the 24-row window, renders with blank padding
**above** the content and the content (+ prompt) bottom-anchored — with
**no `~` tilde fill lines at all**. Confirmed identically across unrelated
fixtures: `startup-single-file`, `prompt-error-bell-default/-partial-quiet/
-full-quiet`, `opt-chop-long-lines`, `opt-wrap-default`,
`default-tabs-and-control-chars`, `opt-raw-control-chars`, `utf8-handling`,
`utf8-malformed`, `startup-multi-file` (file1-initial only).

Contrast: `startup-multi-file`'s **second** screen (after `:n` to file2, also
a 5-line file) *is* top-anchored with `~` tilde fill below — the standard
"pager" rendering. So the no-clear/bottom-anchor/no-tilde rendering is
specific to the very first draw of a fresh less process, not a general
short-file behavior. `opt-line-numbers-longline-interaction` shows this most
starkly: with a 5-logical-line, wrap-producing-9-rows file, the first draw
shows only the *tail* of line 4's wrap + line 5 + prompt (rows 19-24) with
rows 1-18 blank — lines 1-3 are not rendered at all, not even off-screen
scrolled-past, just never drawn. This is a **high-impact, undocumented,
non-obvious startup behavior** — a naive implementation will almost
certainly get this wrong (most pagers/editors intuitively top-anchor).
Recommend Phase 4 treat this as a first-class spec item, not an edge case.

### -R silently drops non-conforming near-ANSI sequences (neither raw nor caret)
`opt-raw-control-chars/ansi.txt` deliberately included a non-color escape
sequence, `ESC[2;5H` (cursor position — digits/`;` are valid
LESSANSIMIDCHARS, but `H` is not the default LESSANSIENDCHARS `m`). Expected
either raw passthrough (documented: no, only `m`-terminated) or caret-notation
fallback (documented: "other control chars still use caret notation"). What
actually happened: the entire `ESC[2;5H` sequence **vanished** — not raw,
not caret-notation, just silently dropped; only the surrounding literal text
survived (`"non-color esc sequence: non-color-esc-seq-here end"`). This
contradicts the literal reading of "other control chars still use caret
notation" for this class of malformed-almost-ANSI sequence. OSC 8
hyperlinks, both `ESC\`-terminated and BEL-terminated, passed through raw
correctly in the same capture. Recommend Phase 4 replicate the
swallow-not-caret behavior for byte-exact parity if this file class matters.

### UTF-8 malformed-sequence fallback uses LESSBINFMT, not LESSUTFBINFMT
`utf8-malformed/`: the truncated 3-byte UTF-8 sequence (`\xe4\xb8`, missing
its final continuation byte) renders as **two separate** `<E4>` `<B8>`
tokens — one per orphaned byte, 2-hex-digit format, no `U+` prefix — and in
the 235/214 highlight color (consistent with LESSBINFMT's documented
`*s<%02X>` default, where `*s` is a standout-start placeholder). This
**contradicts** `declared-inventory.json`'s `utf8-handling.exact_values.
utf8_bin_fmt_default: "<U+%04lX>"`, which is LESSUTFBINFMT's format — that
format is for *valid* UTF-8 codepoints that aren't printable, not for
*invalid/truncated* byte sequences, which fall back to the generic
LESSBINFMT byte-hex format instead. **This is a real
man-page-vs-declared-inventory mismatch worth a note in the ledger** (see
below) — Phase 4 must use per-byte `<%02X>` for malformed sequences, not
`<U+%04lX>`.

### -m / -M prompt format strings confirmed exactly, including EOF asymmetry
`prompt-medium-M/`: percent (`36%`) shown mid-file, **dropped** at EOF
(`(END)` alone) — because `?e(END)` short-circuits before the percent
branch in the `-m` format string. `prompt-long-M/`: `lines A-B/L` **persists**
at EOF (`lines 178-200/200 (END)`) because the `lines` segment is emitted
before the `?e(END)` branch in the `-M` format string. Both exactly match
the documented format strings char-for-char.

### Search-match highlight scope not fully verified
Confirmed: a found match is rendered in the 235/214 highlight pair. **Not**
verified: whether *all* on-screen occurrences get highlighted simultaneously
(vs. only the one just jumped to) — no scenario has 2+ occurrences of the
same search term visible within a single 24-row screen at once. Left as
`unconfirmed-lead` in the ledger for that specific sub-claim.

### Bell/audio claims are structurally unverifiable by this capture method
`prompt-error-bell-{default,partial-quiet,full-quiet}/`: all three (no flag,
`-q`, `-Q`) produce byte-identical on-screen output for the past-EOF `j`
trigger (only the filename dropping from the 2nd prompt, unrelated to the
bell). `capture-pane` cannot observe audio bells or a transient visual-bell
flash that reverts before the next capture. This claim category should be
considered permanently `unconfirmed-lead` under this harness design, not a
gap to close later — flagging so Phase 5 doesn't waste a retry loop on it.

### Environment-color provenance caveat
The 235/214 (dark-grey-on-amber) highlight color appears consistently across
error messages, search highlights, and the filename prompt — consistent
enough to be a genuine built-in default of less 668 on this system rather
than random contamination. However, `CONTRACT.md`'s env scrub
(`-u LESS -u LESSOPEN -u LESSCLOSE`) does not unset `LESS_TERMCAP_*`,
which is a common shell-dotfile customization point for exactly this kind of
coloring. **Not re-verified independently** (read-only task, no tmux
access) — flagging as a provenance risk for whoever owns CONTRACT.md: if a
future re-capture on a different machine produces different color codes for
these same elements, check `LESS_TERMCAP_*`/`LESSCOLORS`/`LESS` env vars
before concluding the *binary version* changed.

## Residual gaps (not covered by this baseline / this harness)

- `z`, `w`, `^D`, `^B`, `ESC-v`, `^Y`, `^P`, `^K`, `<`, `ESC-<`, `>`, `ESC->`,
  `=` (file-info message), `-G` (suppress highlight), `-K`/`^C` (interrupt
  exit code 2) — no scenario exercises these; ledger entries for them remain
  `unconfirmed-lead`.
- Half-window rounding for an **odd** window height — both harness pane
  sizes (80x24, 100x30) are even; genuinely untested.
- Resize repaint *mechanism* (byte-level redraw sequence) — capture-pane
  shows end-state only, not a transcript; would need `tmux pipe-pane` to
  observe.
- `LESS_LINES`/`LESS_COLUMNS`, `-j`/`-#` fraction-of-screen resize
  recalculation — no scenario sets these.

## Ledger update summary

`docs/evidence-ledger.json` has 48 entries. Applied (verification field only,
every other field byte-identical, verified programmatically):

- **confirmed-by-capture: 36** (32 were `unconfirmed-lead`, 4 were the
  `capture-determined` items this capture set targeted — arrow/PgUp/PgDn
  bindings, search-not-found message text, normal-quit exit code, and
  prompt reverse-video/standout).
- **contradicted-by-capture (within evidence-ledger.json itself): 0.** No
  ledger.json claim was directly falsified by the baseline.
- **left unchanged: 12** — 11 stay `unconfirmed-lead` (genuinely untested:
  `z`, `w`, `^D`/`^B`/`ESC-v` variants, `^Y`/`^P`/`^K` variants, `<`/`ESC-<`/
  `>`/`ESC->` variants, "ALL matches highlighted" scope, `-G`, `-K`/`^C` exit
  code 2, `=` message, bell-ringing itself, `-j`/`-#`/`LESS_LINES`/
  `LESS_COLUMNS` resize recalculation — see "Residual gaps" above for the
  full breakdown); 1 stays `capture-determined` (the resize repaint
  *mechanism/timing* claim — the *outcome* is confirmed, per item (e) above,
  but capture-pane snapshots can't show the redraw transcript itself, so the
  claim as worded is not fully resolved).

**One real contradiction exists, but against a different file:**
`docs/declared-inventory.json`'s `utf8-handling.exact_values.
utf8_bin_fmt_default: "<U+%04lX>"` does not match what the baseline actually
shows for malformed UTF-8 (`<E4>`/`<B8>`, LESSBINFMT-style, see above). That
field is outside the scope of this task (only `evidence-ledger.json`
verification was in scope) — flagging here for whoever owns
`declared-inventory.json` to correct or annotate.

## Addendum (hub, 2026-08-08): half-window rounding resolved

Scenario `nav-halfwindow-oddheight` (80x25, odd height) measured `d` scrolling
13 lines (top 1→14), `u` symmetric (27→14). Combined with even-height data
(24→12, 30→15): **half_window = ceil(terminal_rows / 2)**. All three data
points consistent. Sticky-N override behavior unchanged (see nav-halfwindow).
