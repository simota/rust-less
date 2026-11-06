# rust-less Reconstruction Spec

Phase 2-3 deliverable. Target: clean-room, behavior-parity Rust reimplementation of the
core-pager subset of GNU less 668, verified against `parity/baseline/` (39 scenarios,
`/usr/bin/less` on Darwin 25.5.0, tmux 3.7b, 80x24 pane, `env -u LESS -u LESSOPEN -u LESSCLOSE
LESSHISTFILE=- LANG=C.UTF-8`). The baseline capture is the sole oracle: no `less` source was
read to produce this document (Rights Record, `.agents/nexus.md`). Every behavioral claim below
cites either a baseline capture file or a specific `docs/evidence-ledger.json` entry; claims
without a citation are not in this spec.

Citation shorthand: `scenario/capture` refers to `parity/baseline/scenario/capture.txt` (and,
where relevant, `capture.attrs.txt`). Row/column numbers are 1-indexed against the pane's actual
dimensions (80x24 unless stated).

## §1 Scope and Non-Goals

### In scope
The 35 `feature_id`s in `docs/declared-inventory.json`, grouped as: startup (file arg, stdin
pipe, multi-file args), vertical navigation (line/window/half-window forward+backward, goto
first/last line, numeric prefix, arrow/PageUp/PageDown bindings), search (forward, backward,
repeat, repeat-reverse, highlight, not-found message), display options (`-N` line numbers, `-S`
chop, `-R` raw ANSI passthrough, default wrap/tab/caret formatting), multi-file commands (`:n`/
`:p`, `(file N of M)` indicator), exit (5 quit commands, exit codes), prompts (initial filename,
short `:` prompt, `(END)` marker, `-m`/`-M` byte/percent/line-range rules, error/bell messages),
terminal resize redraw, ANSI/OSC8 escape handling, and UTF-8 (valid multi-byte + malformed
fallback). §2 covers all 35 by `feature_id`.

### Explicit non-goals (deferred)
The 12 features in `docs/declared-inventory.json`'s `deferred` array are out of scope for this
spec and for Phase 4: **mark (`m`/`'`), follow (`F`), filter (`&`), `-i`, `-I`, `+cmd`, help
screen (`h`), editor (`v`), horizontal scroll (`ESC-(` / `ESC-)`), `-p`, `-F`, `-X`**. No baseline
scenario exercises them; implementing them is not required for Phase 4/5 parity.

**Extensions after Phase 5 (not parity-verified).** Most of the deferred features above now
exist as rless's own extensions: mark (`m`/`'`, plus `''`), follow (`F`, `+F`), filter (`&`,
`&!`), `-i`/`-I` (also typed in the pager, with `-N` and `-S`), `+cmd`, `-p`, `-F`, `-X`, a help
screen (`h`, this project's own text, not less's), and horizontal scroll (Right/Left, `ESC-)`/
`ESC-(`, under `-S` and in the table view). No baseline scenario covers them, so they are
specified by `cargo test` and `docs/fvi/fvi_probe.py`, not by the less oracle; known
differences from less: any key (not only the interrupt key) stops `F`, `-F` prints the rendered
input instead of drawing it, and `$LESS` contributes only the flags rless implements. The editor
(`v`) stays out of scope. None of these keys or flags appears in a parity scenario, and
`make parity` stays 40/40.

Horizontal scroll was out of scope for parity **even under `-S`**: `opt-chop-long-lines` (feature
`opt-chop-long-lines-S`) verifies only that long lines are truncated with a trailing `>` marker
(§2, `opt-chop-long-lines`); the `RIGHT-ARROW` reveal-of-chopped-content behavior that
`declared-inventory.json`'s `fixture_needs` mentions is not captured by any scenario and is
folded into the same deferred/non-goal bucket as horizontal scroll generally.

Bell behavior (`prompt-error-bell` feature, `-q`/`-Q`/default) is declared **best-effort,
permanently unverifiable** by this harness: `capture-pane` records only the on-screen cell grid,
never an audible bell or a transient visual-bell flash that reverts before the next capture.
`prompt-error-bell-default/-partial-quiet/-full-quiet` are screen-identical for `after-j-past-eof`
across all three variants (§2, `prompt-error-bell`) — confirming only that no *visible* text
differs, not whether a bell rang. Phase 4 should ring `\x07` (or substitute a visual flash under
`-Q`) per the man-page rule cited in the ledger, but Phase 5 diffing cannot and should not gate on
it (see §5, UNVERIFIED).

### Parity ceilings (inherited from SDR, `.agents/nexus.md`)
1. **Regex dialect** — POSIX BRE/ERE (real less) vs. Rust `regex` crate (ERE-like, no
   backreferences). Both tested patterns in `search-regex-basic` (`[cb]at` character class,
   `foo.*456` greedy dot-star) use only common ERE syntax with no POSIX-specific extension and no
   backreference, so full behavioral parity is expected and required for those two; more exotic
   POSIX constructs are a declared allowable divergence, not exercised by any baseline scenario.
2. **crossterm terminal-init sequence** — not compared. `CONTRACT.md`'s `.txt` diff is a
   post-`capture-pane` cell-grid snapshot, which never contains the raw init escape stream in the
   first place, so this ceiling is currently a no-op under the existing harness scope; it is
   restated here only because the SDR declares it and a future escape-transcript-level test would
   need it.

## §2 Feature-by-feature spec

### startup-file-arg
**Behavior:** `less <file>` displays the file's content starting from the top. **Boundary
(high-impact, undocumented):** when the file's rendered content (after wrap) is *shorter* than
the window, the very first draw of a fresh process is **bottom-anchored with blank padding above
and no `~` tilde fill** — not top-anchored. Confirmed identically across `startup-single-file`,
`prompt-error-bell-default/-partial-quiet/-full-quiet`, `opt-chop-long-lines`, `opt-wrap-default`,
`default-tabs-and-control-chars`, `opt-raw-control-chars`, `utf8-handling`, `utf8-malformed`, and
`startup-multi-file`'s `file1-initial` only (5-line fixtures in a 24-row pane).
`opt-line-numbers-longline-interaction/initial-longline-with-numbers` shows the extreme case: a
5-logical-line file that wraps to 9 display rows renders rows 19-24 only (tail of line 4's wrap +
line 5 + prompt); rows 1-18 are blank and lines 1-3 are **never drawn**, not scrolled-past.
**Contrast:** `startup-multi-file/after-colon-n-to-file2` (second file opened via `:n`, also
5 lines) *is* top-anchored with `~` fill below — the bottom-anchor/no-tilde behavior is specific
to the first draw of a fresh process, not a general short-file rule. For files taller than the
window (`startup-large-file`, 5000-line fixture), the first screen is ordinary top-anchored
content: rows 1-2 verbatim `00001 large file line 1 of 5000 :: filler content` /
`00002 large file line 2 of 5000 :: filler content`; bottom row is just the filename
(`parity/fixtures/large.txt`), no byte/percent (default short prompt never shows percent
regardless of file size — see `prompt-byte-percent-rules`).
**Source:** `startup-single-file/first-screen`, `startup-large-file/first-screen`,
`opt-line-numbers-longline-interaction/initial-longline-with-numbers`.

### startup-stdin-pipe
**Behavior:** piping content in (`cat file | less`) renders identical content to the equivalent
file argument, but the prompt row **omits the filename** entirely (bare `(END)` instead of
`parity/fixtures/short.txt (END)`), since `%f` has no value for stdin. **Source:**
`startup-stdin-pipe/first-screen` vs. `startup-single-file/first-screen`.

### startup-multi-file-args
**Behavior:** multiple filenames load into an ordered list; the first is shown immediately, the
rest reachable via `:n`/`:p`. **Source:** `startup-multi-file/file1-initial` (see also
`multifile-next-prev`, `multifile-prompt-indicator` below for the detailed prompt/boundary
behavior this feeds).

### nav-forward-line
**Behavior:** `j`/`^N`/`e`/`^E`/Enter/`^J` scroll forward N lines, default 1; full N lines move
even if N exceeds window height. **Exact values (`nav-line-forward-backward`):** top-of-window
sequence 1 → (`jjj`) 4 (+3) → (`kk`) 2 (−2) → (Enter) 3 (+1) → (`5j`) 8 (+5) → (`3k`) 5 (−3) →
(`50j`) 55 (**+50**, window is only 23 content rows — confirms full-N move) → (`20k`) 35 (−20).
**Boundary:** `nav-boundary-eof-bof/after-300j-clamped-to-eof` clamps exactly at EOF (top=178,
bottom=200, row 24 `(END)`); a further `j` (`j-at-eof-noop`) is **byte-identical** to the prior
capture — true no-op, no error text (bell-only, see `prompt-error-bell`).
**Source:** `nav-line-forward-backward/*`, `nav-boundary-eof-bof/*`.

### nav-backward-line
**Behavior:** `y`/`^Y`/`^P`/`k`/`^K` scroll backward N lines, default 1. Same sequence as above:
`kk` = −2, `3k` = −3, `20k` = −20. **Boundary:** `nav-boundary-eof-bof/k-at-bof-noop` — diffing
against `initial` shows only row 24 changes (filename banner → bare `:`, the standard
first-prompt-to-second-prompt transition, see `prompt-initial-filename`); content rows 1-23 are
unchanged — `k` at BOF is a true content no-op. **Source:** `nav-line-forward-backward/*`,
`nav-boundary-eof-bof/{initial,k-at-bof-noop}`.

### nav-forward-window
**Behavior:** SPACE/`^V`/`f`/`^F` scroll forward N lines, default one window (= content-row
count). **Exact values (`nav-window-forward-backward`, window = 23 content rows on 80x24):**
top sequence 1 → (Space) 24 (+23) → (Space) 47 (+23) → (`b`) 24 (−23) → (`10f`) 34 (+10, N <
window) → (`50f`) 84 (**+50**, N > window — the final resting position still moves the full N;
"only the final screenful is displayed" governs the draw process, not the landing line).
**Source:** `nav-window-forward-backward/*`.

### nav-backward-window
**Behavior:** `b`/`^B`/`ESC-v` scroll backward N lines, default one window. Same scenario:
(`10b`) 74 (−10) → (`50b`) 24 (−50, N > window, still moves full N). **Source:**
`nav-window-forward-backward/*`.

### nav-forward-halfwindow
**Behavior:** `d`/`^D` scroll forward N lines, default one half-window; an explicit N becomes the
new **sticky default** shared with `u`. **Exact rounding (resolved, hub addendum in
`docs/capture-findings.md`):** `half_window = ceil(rows / 2)`. Even height 24 → 12
(`nav-halfwindow`: top 1 → 13 → 25, i.e. +12 each `d`). Odd height 25 → 13 (`nav-halfwindow-
oddheight`: top 1 → 14 → 27, i.e. +13 each `d`, confirmed by direct read of
`nav-halfwindow-oddheight/{initial-25rows,after-d-odd-height,after-d-again-odd-height}`).
**Sticky-N (`nav-halfwindow`):** `5d` sets default=5 (top 13 → 18, +5); a subsequent bare `d`
uses the sticky 5 (18 → 23, +5). **Source:** `nav-halfwindow/*`, `nav-halfwindow-oddheight/*`.

### nav-backward-halfwindow
**Behavior:** `u`/`^U` scroll backward N lines, default one half-window, sharing the sticky
default with `d`. **Exact values:** `nav-halfwindow`: bare `u` after two `d`s: 25 → 13 (−12,
default half-window). After `5d` sets sticky=5: bare `u` 23 → 18 (−5). Odd-height:
`after-u-odd-height` 27 → 14 (−13, symmetric with the odd-height `d`). **Source:**
`nav-halfwindow/{after-u-back-half,after-bare-u-uses-sticky-5}`, `nav-halfwindow-oddheight/
after-u-odd-height`.

### nav-goto-first-line
**Behavior:** `g`/`<`/`ESC-<` go to line N, default 1. **Exact values (`nav-goto-line`):**
`after-g-goto-first` → top=1. **Boundary (`nav-numeric-prefix-edge`):** `0g` → top=1, **no
error** (clamped, same as bare `g`) — `after-0g-zero-edge-case`. **Source:**
`nav-goto-line/after-g-goto-first`, `nav-numeric-prefix-edge/after-0g-zero-edge-case`.

### nav-goto-last-line
**Behavior:** `G`/`>`/`ESC->` go to line N, default the last line. **Exact values
(`nav-goto-line`, 200-line fixture):** `after-G-goto-last` → top=178, bottom=200, row 24 `(END)`.
**Boundary, ambiguity resolved:** `after-50G-mid-file` → top=**50**, identical to `50g` — a
numeric prefix on `G` means "go to line N", it does **not** make `G` ignore the prefix and jump
to EOF. **Source:** `nav-goto-line/{after-G-goto-last,after-50G-mid-file}`.

### nav-numeric-prefix
**Behavior:** commands marked `*` accept a preceding decimal N. **Boundary, high-impact
asymmetry:** an out-of-range **absolute** target (`g`/`G`) does **not** clamp — it errors and
leaves position unchanged: `nav-goto-line/after-9999g-beyond-eof-clamped` (name is misleading;
actual result) shows top **unchanged** from the prior 100 and row 24 shows literal
`Cannot seek to line number 9999  (press RETURN)` (two spaces before the parenthetical, 235/214
highlight). `nav-numeric-prefix-edge/after-huge-number-g` (`999999999999g`) shows the identical
pattern (`Cannot seek to line number 999999999999  (press RETURN)`, position unchanged at 1). This
is the **opposite** of relative-scroll clamping (`nav-forward-line`'s `300j` clamps cleanly to
EOF with no error). Implementations must not silently clamp `g`/`G` out-of-range targets.
**Source:** `nav-goto-line/after-9999g-beyond-eof-clamped`, `nav-numeric-prefix-edge/
after-huge-number-g`.

### nav-arrow-pgup-pgdn-bindings
**Behavior (fully capture-determined, undocumented in man/`--help`):** Down=`j` (+1 line),
Up=`k` (−1 line), PageDown(NPage)=`f` (+1 window), PageUp(PPage)=`b` (−1 window). **Exact values
(`nav-arrow-pgup-pgdn`, starting at `50g`, window=23):** Down 050→051, Up 051→050, NPage 050→073,
PPage 073→050. **Source:** `nav-arrow-pgup-pgdn/{initial-at-line-50,after-down-arrow,
after-up-arrow,after-pagedown,after-pageup}` (per `docs/capture-findings.md` item (a)).

### search-forward
**Behavior:** `/pattern` searches forward for the N-th matching line (default 1), case-sensitive,
starting at the first displayed line. On match, the matched line is placed at the **top** of the
window. **Exact values:** `search-forward-basic` — `SOLO_NEEDLE` (unique at fixture line 111)
lands at top=111. **Regex engine parity (`search-regex-basic`):** `/[cb]at` matches fixture line
60 (`060 line 60 of 200 :: regex sample cat bat hat sat mat`), highlighting both `cat` (cols
36-38) and `bat` (cols 40-42) — confirms ERE character-class support. `/foo.*456` matches fixture
line 61 (`061 line 61 of 200 :: regex sample foobar123 foobaz456`) with the match spanning the
**entire** `foobar123 foobaz456` (cols 36-54) — confirms standard greedy `.*` (does not stop at
first `456`-adjacent boundary). Rust `regex` crate must reproduce both exactly (declared in
scope per the regex parity ceiling in §1). **Source:** `search-forward-basic/
after-search-single-match`, `search-regex-basic/{after-charclass-regex,after-dotstar-regex}`.

### search-backward
**Behavior:** `?pattern` searches backward for the N-th matching line, starting at the last
displayed line. **Exact values (`search-backward-basic`):** starting at `G` (top=178, EOF), `?
SOLO_NEEDLE` lands at top=111 — same top-of-window landing convention as forward search.
**Source:** `search-backward-basic/{initial-at-eof,after-backward-search}`.

### search-repeat
**Behavior:** `n` repeats the last search, same direction, N-th next occurrence. **Exact values
(`search-forward-multi-match-repeat`, `ALPHA_TOKEN` at fixture lines 20/80/140):** `/ALPHA_TOKEN`
→ top=20; `n` → top=80; `n` → top=140. **Source:** `search-forward-multi-match-repeat/
{after-first-match,after-n-second-match,after-n-third-match}`.

### search-repeat-reverse
**Behavior:** `N` repeats the last search in the reverse direction. **Exact values:** continuing
the sequence above, after reaching top=140 (third `ALPHA_TOKEN`), `N` → top=**80** (back to the
second match, not a fresh backward search from the current position but the reverse walk of the
same match list). **Source:** `search-forward-multi-match-repeat/after-N-reverse-back-to-second`.

### search-highlight-matches
**Behavior:** by default, **all** on-screen occurrences of the last search pattern are
highlighted simultaneously, not only the one just landed on. **Resolution of a prior
`unconfirmed-lead`:** `docs/capture-findings.md` flagged this sub-claim unverified because no
scenario it inspected had 2+ occurrences visible at once. Direct re-inspection of
`search-regex-basic/after-charclass-regex.attrs.txt` (not originally examined for this question)
shows **two independent** `ESC[38;5;235mESC[48;5;214m...ESC[39mESC[49m` spans on the same row —
`cat` (cols 36-38) and `bat` (cols 40-42) — both highlighted from a single `/[cb]at` search that
matched twice on one visible line. This **confirms** the "highlight all on-screen matches" rule
via direct capture evidence, upgrading it beyond the ledger's `unconfirmed-lead` status for this
specific sub-claim (the `-g`/`-G` option claims themselves remain `unconfirmed-lead`, untested,
per `docs/evidence-ledger.json`). **Highlight rendering:** the 235/214 pair (dark grey
foreground / amber background via explicit `ESC[38;5;235m`/`ESC[48;5;214m`, reset via
`ESC[39mESC[49m`) — not a plain `ESC[7m` reverse-video toggle. **Source:**
`search-regex-basic/after-charclass-regex.attrs.txt`, `search-forward-basic/
after-search-single-match.attrs.txt`.

### search-not-found-message
**Behavior/exact text:** `Pattern not found  (press RETURN)` (two spaces before the
parenthetical), on row 24 (bottom status line), replacing the normal prompt, in the 235/214
highlight pair. Confirmed both directions. **Source:** `search-not-found/
{after-forward-search-fail,after-backward-search-fail}` (`.txt` + `.attrs.txt`).

### opt-line-numbers-N
**Behavior/exact format:** `-N` prefixes every displayed row with a **7-character,
right-justified, space-padded** line number field followed by **exactly one space** separator,
then the raw line content — width is fixed at 7 regardless of digit count (`      1 001 line 1…`,
`     50 050 line 50…`). **Boundary — wrap interaction:** on a wrapped logical line, the number
field is **repeated on every continuation row**, not just the first
(`opt-line-numbers-longline-interaction`: all 5 wrapped rows of the 316-char logical line 4 show
`      4 ` at the start). `-n` (default) suppresses the field entirely. **Source:**
`opt-line-numbers/{initial-with-line-numbers,mid-file-with-line-numbers}`,
`opt-line-numbers-longline-interaction/initial-longline-with-numbers`.

### opt-chop-long-lines-S
**Behavior:** `-S` truncates lines longer than the terminal width (80 cols) instead of wrapping;
no continuation rows are produced. **Exact marker:** a line exactly 80 chars renders unmarked and
full-width; a line longer than 80 chars is truncated to exactly 80 columns with the **final
column replaced by a literal `>`** (e.g. an 81-char source line renders as 79 chars of content +
`>`). **Source:** `opt-chop-long-lines/chopped-view` (rows for source lines of length 80/81/316,
all truncate to 80 cols, `>` marker present only when source > 80).

### opt-raw-control-chars-R
**Behavior:** `-R` passes ANSI color (`ESC[...m`) and OSC 8 hyperlink sequences through raw;
color resets at the start of every line regardless of an unclosed sequence on the prior line.
**Confirmed (`opt-raw-control-chars`, `compare_attrs: true`):** red/bold-green/blue segments
render with matching `ESC[31m`/`ESC[1m ESC[32m`/`ESC[34m` (terminated `ESC[39m`); an unclosed
`ESC[35m` (magenta) on one fixture line does not bleed into the next line's rendering (per-line
reset, matches man-page rule). **Boundary — malformed near-ANSI sequences vanish silently:** a
non-`m`-terminated sequence (`ESC[2;5H`, a cursor-position code — digits/`;` are valid
LESSANSIMIDCHARS but `H` is not the default LESSANSIENDCHARS `m`) is **dropped entirely** —
neither passed raw nor rendered as caret notation — only the surrounding literal text survives
(`non-color esc sequence: non-color-esc-seq-here end`). This contradicts a literal reading of "no
n-color control chars still use caret notation"; Phase 4 must replicate the silent-drop, not
substitute caret notation, for parity on this fixture class. OSC 8 hyperlinks pass through raw in
both terminator forms present in the fixture (`ESC\`-terminated and BEL-terminated source
sequences) — see §3 for a caveat on the BEL-terminator's exact representation in the capture.
**Boundary — without `-R`:** `opt-raw-control-chars-without-R` shows `-R`'s absence does **not**
render caret/literal-escape text; instead less detects binary content up front and shows only
`"parity/fixtures/ansi.txt" may be a binary file.  See it anyway?` (row 1, rest blank, same
double-space convention). The harness's `q` keystroke is consumed as a **"no" answer to this
binary-file confirmation gate**, not as an ordinary quit — the process exits with **code 1**
(not 0), despite the scenario's `meta.json` declaring `expect_exit: 0`. This is a genuine,
capture-confirmed exit-code divergence Phase 4 must reproduce: opening a file containing raw
control/escape bytes without `-R` must trigger the binary-file confirmation prompt, and declining
it must exit 1. **Source:** `opt-raw-control-chars/{ansi-raw-view.txt,ansi-raw-view.attrs.txt}`,
`opt-raw-control-chars-without-R/{ansi-without-R-view.txt,exit_code,steps.log}`.

### default-display-formatting
**Behavior:** default (no `-S`) wraps long lines onto continuation rows at column 80 with **no
leading indicator** on continuation rows (flush at column 1). **Exact values
(`opt-wrap-default`):** an 81-char line wraps to 80 + 1 chars across two rows; a 316-char line
wraps across 4 rows (80+80+80+76). **Tabs:** 8-column stops, confirmed via
`default-tabs-and-control-chars/tabs-and-ctrl-view` — a line with `a\tb\tc\td\t` renders `a` at
col 1, `b` at col 9, `c` at col 17, `d` at col 25 (stops at columns 1, 9, 17, 25, …); a
leading-tab line indents content to col 9 (8 spaces). **Control chars:** caret notation confirmed
for `\x01 \x02 \x07 \x1f` → `^A ^B ^G ^_` respectively (`ctrl:[^A][^B][^G][^_]end`). The
hex-fallback path (`LESSBINFMT` default `*s<%02X>`, for bytes whose 0100-bit inversion is not
printable) is **not independently re-observed** by this fixture (all four tested control bytes
happen to invert to printable ASCII); its existence is a `confirmed-by-capture` ledger claim for
the *rule*, sourced from the man page's NATIONAL CHARACTER SETS section, not from a byte that
actually exercises the hex branch — Phase 4 should still implement the hex-fallback branch per
that ledger entry, but be aware no baseline scenario byte-verifies its exact rendering.
**Source:** `opt-wrap-default/wrapped-view`, `default-tabs-and-control-chars/
tabs-and-ctrl-view`, `docs/evidence-ledger.json` (control-char/LESSBINFMT entry).

### multifile-next-prev
**Behavior:** `:n` examines the next file in the command-line list (or N-th next with a count);
`:p` examines the previous (or N-th previous). **Exact values (`multi-file-count-nav`, 3-file
list):** `2:n` from file 1 jumps directly to file 3 (**skips** file 2); `2:p` from file 3 jumps
back to file 1 (skips file 2 again) — confirms `N:n`/`N:p` count files, not lines or matches.
**Boundary (`startup-multi-file`):** `:n` at the last file (file 3 of 3) produces
`No next file  (press RETURN)` (235/214 highlight, same two-space convention); `:p` at the first
file produces `No previous file  (press RETURN)`, symmetrically. **Source:**
`multi-file-count-nav/{initial-file1,after-2colon-n-to-file3,after-2colon-p-to-file1}`,
`startup-multi-file/{after-colon-n-at-last-boundary,after-colon-p-at-first-boundary}` (boundary
message text per `docs/capture-findings.md`).

### multifile-prompt-indicator
**Behavior:** the short-prompt escape `?m(%T %i of %m).` renders `(file N of M)` whenever more
than one input file is open. **Exact values:** `startup-multi-file/file1-initial` row 24 (full,
untruncated logical text) is
`parity/fixtures/multi-1.txt (file 1 of 3) (END) - Next: parity/fixtures/multi-2.txt` (83 chars).
**Boundary, exact — prompt truncates from the LEFT, not the right:** on an 80-col pane this
83-char string does not fit; the actual rendered row 24 is
`ity/fixtures/multi-1.txt (file 1 of 3) (END) - Next: parity/fixtures/multi-2.txt` — the leading
3 characters (`par`) are dropped, keeping the tail (including the trailing `Next:` segment)
visible. Directly re-verified byte-for-byte against `multi-file-count-nav/initial-file1.txt`
(same fixture set, identical truncated string). When the message fits (file 3 of 3, no `Next:`
segment, 47 chars), no truncation occurs: `multi-file-count-nav/after-2colon-n-to-file3` row 24 =
`parity/fixtures/multi-3.txt (file 3 of 3) (END)`, untruncated. Phase 4's prompt renderer must
truncate long prompt lines by dropping leading characters, not trailing ones. **Source:**
`startup-multi-file/file1-initial`, `multi-file-count-nav/{initial-file1,
after-2colon-n-to-file3}` (row 24 of each, byte-verified directly).

### exit-commands
**Behavior:** `q`, `Q`, `:q`, `:Q`, `ZZ` all exit less. **Source:** `exit-{lower-q,upper-q,
colon-lower-q,colon-upper-q,ZZ}/before-quit` (screen state prior to each quit key, confirming the
pager was in a normal state when each form was invoked).

### exit-codes
**Behavior/exact values:** all five quit forms above exit with code **0**
(`exit-{lower-q,upper-q,colon-lower-q,colon-upper-q,ZZ}/exit_code`). File-not-found exits with
code **1** (`exit-code-file-not-found/exit_code`) — immediately, fast enough that the capture
harness's first step raced past the pane's disappearance (handled by the harness's WARN path, no
`error-view` content captured). The binary-file-confirmation-declined path (see
`opt-raw-control-chars-R` above) also exits **1** — a second, distinct source of exit code 1
Phase 4 must reproduce, not just the file-not-found path. `-K`/`--quit-on-intr` producing exit
code 2 on interrupt is **documented but not baseline-verified** (`unconfirmed-lead`, no scenario
sends `^C`/tests `-K`) — implement per the man-page claim in `docs/evidence-ledger.json`, but it
is not a Phase 5 diff target (see §5). **Source:** `exit-*/exit_code` (5 files, all `0`),
`exit-code-file-not-found/exit_code` (`1`), `opt-raw-control-chars-without-R/exit_code` (`1`).

### prompt-initial-filename
**Behavior:** the escape `?n?f%f.` shows the filename only on the first prompt of a newly opened
file. **Exact values:** `prompt-initial-filename/first-prompt-with-filename` shows the filename
(e.g. `parity/fixtures/lines200.txt`) rendered in the 235/214 highlight pair (`compare_attrs:
true`). After any movement (`j`), `second-prompt-no-filename` shows a **bare `:`** with **zero**
attrs (plain text, no color/standout) — the filename-highlight styling applies only to that
special first-prompt content, not to the ordinary colon prompt. **Source:**
`prompt-initial-filename/{first-prompt-with-filename,second-prompt-no-filename}` (`.txt` +
`.attrs.txt`).

### prompt-short-colon
**Behavior/exact format string:** `?n?f%f .?m(%T %i of %m) ..?e(END) ?x- Next\: %x..%t` — this
is the format whose rendered instances are documented above under `prompt-initial-filename`
(filename segment), `multifile-prompt-indicator` (`(file N of M)` segment), and
`prompt-end-marker` ((END) segment). **Attrs rule:** plain colon prompt = zero attrs; filename/
error/boundary-message content = 235/214 highlight (see `prompt-initial-filename`). **Source:**
`docs/evidence-ledger.json` (format-string claim, `confirmed-by-capture`); rendering instances
per the three features above.

### prompt-end-marker
**Behavior/exact text:** `(END)` appears at end-of-file. **Exact values:** `nav-goto-line/
after-G-goto-last` row 24 = `(END)` (bare, single-file, no other segments); `nav-boundary-eof-bof/
after-300j-clamped-to-eof` row 24 = `(END)`; `startup-multi-file/after-colon-n-to-file3` row 24 =
`parity/fixtures/multi-3.txt (file 3 of 3) (END)` (combined with the multi-file indicator, no
trailing `Next:` since it's the last file). **Source:** as cited inline above.

### prompt-byte-percent-rules
**Behavior/exact format strings:** medium (`-m`):
`?n?f%f .?m(%T %i of %m) ..?e(END) ?x- Next\: %x.:?pB%pB\%:byte %bB?s/%s...%t`; long (`-M`):
`?f%f .?n?m(%T %i of %m) ..?ltlines %lt-%lb?L/%L. :byte %bB?s/%s. .?e(END) ?x- Next\: %x.:?pB%pB\
%..%t`. **Exact rendered values, `-m` (`prompt-medium-M`, 200-line fixture):** initial (top=1) =
`parity/fixtures/lines200.txt 11%`; mid-file (after `50g`, top=50) = `36%` (filename dropped once
past the first prompt, per `prompt-initial-filename`); EOF (after `G`) = `(END)` — **percent is
dropped at EOF** because `?e(END)` short-circuits before the percent branch. **Exact rendered
values, `-M` (`prompt-long-M`):** initial = `parity/fixtures/lines200.txt lines 1-23/200 11%`;
mid-file = `parity/fixtures/lines200.txt lines 50-72/200 36%`; EOF =
`parity/fixtures/lines200.txt lines 178-200/200 (END)` — **the `lines A-B/L` segment persists at
EOF** (asymmetric with `-m`) because it's emitted before the `?e(END)` branch in the `-M` format
string. **Source:** `prompt-medium-M/{initial-medium-prompt,mid-file-medium-prompt,
eof-medium-prompt}`, `prompt-long-M/{initial-long-prompt,mid-file-long-prompt,eof-long-prompt}`.

### prompt-error-bell
**Behavior:** see §1 non-goals — bell itself is unverifiable. **Confirmed (screen-only):** the
visible `after-j-past-eof` state (bottom row `(END)`) is **byte-identical** across
`prompt-error-bell-default` (no flag), `prompt-error-bell-partial-quiet` (`-q`), and
`prompt-error-bell-full-quiet` (`-Q`), including `.attrs.txt`. This confirms `-q`/`-Q` affect only
the (inaudible-to-capture) bell, never on-screen text. **Source:** `prompt-error-bell-{default,
partial-quiet,full-quiet}/after-j-past-eof` (`.txt` + `.attrs.txt`, diffed pairwise, zero
differences).

### terminal-resize-redraw
**Behavior:** the scrolling window auto-adjusts on resize; position is preserved, not
recentered. **Exact values (`terminal-resize-redraw`, 80x24 → 100x30 via `tmux resize-window`,
positioned at top=50 first):** `before-resize-mid-file` top=050; `after-resize-100x30` top=**050**
(unchanged) with window now 29 content rows (30 total − 1 prompt row, vs. 23 before); a subsequent
`j` (`after-resize-then-scroll`) advances normally to 051. **UNVERIFIED (structural):** only the
end state is observable via `capture-pane`; the byte-level redraw transcript (full clear vs.
incremental repaint) cannot be determined by this harness — see §5. **Source:**
`terminal-resize-redraw/{before-resize-mid-file,after-resize-100x30,after-resize-then-scroll}`.

### ansi-escape-handling
**Behavior:** see `opt-raw-control-chars-R` above for the full ANSI/OSC8/malformed-sequence
behavior — this feature_id is the general "ESC-prefixed handling" umbrella and its baseline
evidence is the same `opt-raw-control-chars` scenario. `ESC` also prefixes two-character commands
(`ESC-v`, `ESC-<`, `ESC->`) per the man page — these specific ESC-prefixed *commands* are not
independently exercised by any scenario beyond their non-ESC equivalents (`b`, `g`, `G` are
captured; the `ESC-` alias forms themselves are `unconfirmed-lead`). **Source:**
`opt-raw-control-chars/*` (as above); `docs/evidence-ledger.json` for the ESC-command aliasing
claim (untested alias forms).

### utf8-handling
**Behavior:** UTF-8 mode auto-detects from `LANG=C.UTF-8` (env canonicalization,
`.agents/nexus.md`); multi-byte and wide (CJK) characters render correctly. **Exact values
(`utf8-handling`, `utf8-mixed.txt`):** a 45-character run of `漢` (U+6F22, 2 display columns each)
wraps after exactly **40** characters (= 80 columns) onto a continuation row holding the remaining
5 — confirms column-width-aware wrapping (via `unicode-width`, already a Cargo dependency), not a
naive 1-char-per-column count. Mixed content renders as expected: `こんにちは世界`, emoji
(`🎉🚀✅`), mixed ASCII+CJK, and narrow accented Latin (`café résumé naïve`) all render without
corruption. **Boundary — malformed UTF-8 uses `LESSBINFMT`, not `LESSUTFBINFMT`
(contradicts `declared-inventory.json`):** `utf8-malformed` (truncated 3-byte sequence `\xe4\xb8`,
missing its final continuation byte) renders as **two separate** tokens `<E4>` `<B8>` — one per
orphaned byte, 2-hex-digit, no `U+` prefix — in the 235/214 highlight color, matching
`LESSBINFMT`'s `*s<%02X>` default (where `*s` is the standout-start placeholder).
`declared-inventory.json`'s `utf8-handling.exact_values.utf8_bin_fmt_default: "<U+%04lX>"` is
`LESSUTFBINFMT`'s format, which applies only to **valid** Unicode codepoints that aren't
printable — **not** to invalid/truncated byte sequences, which fall back to the generic
per-byte `LESSBINFMT` format instead. Per this spec's rule (baseline over declared-inventory when
they conflict), Phase 4 must implement per-byte `<%02X>` for malformed sequences. **Source:**
`utf8-handling/utf8-view`, `utf8-malformed/malformed-utf8-view` (`docs/capture-findings.md`
documents this contradiction; independently confirmed here).

## §3 Implementation Pitfalls

All items below are drawn from `docs/capture-findings.md`'s findings plus the direct
re-verification performed while writing this spec; each restates the concrete trap for Phase 4.

1. **Fresh-launch bottom-anchor, not top-anchor.** The single highest-risk item: a naive
   implementation will top-anchor and tilde-fill on first draw, exactly like `vi`/most pagers.
   Real less does this **only after the first draw of a fresh process**, when content is shorter
   than the window (`startup-file-arg` above). Implement a one-shot "first draw of this process"
   flag in the `command` state machine, not a per-file or per-redraw flag (the *second* file
   opened via `:n` in the same process top-anchors normally).
2. **`half_window = ceil(rows / 2)`**, not floor, not the content-row count. Verified on both an
   even (24 → 12) and odd (25 → 13) pane height; do not derive it from the 23-content-row window
   value used by `nav-forward-window` (that would give a different, wrong number).
3. **Absolute-jump out-of-range errors instead of clamping**; relative-scroll out-of-range
   clamps instead of erroring. `g`/`G` with N beyond EOF produce
   `Cannot seek to line number <N>  (press RETURN)` and leave position unchanged; `j`/`d`/`f`
   etc. with a count that overshoots EOF simply clamp to the last screenful. These are two
   different code paths in less and must be two different code paths in `rust-less`.
4. **Prompt truncation drops leading characters, not trailing ones**, when the rendered prompt
   text exceeds terminal width (`multifile-prompt-indicator` above). A naive `format!()` +
   `.chars().take(width)` implementation truncates the wrong end and will fail parity on any
   long multi-file prompt.
5. **Two distinct sources of exit code 1**: file-not-found, and declining the "may be a binary
   file. See it anyway?" prompt (which is itself triggered by opening a file with raw
   control/escape bytes **without** `-R`). Both must exit 1; ordinary `q`/`Q`/`ZZ`/`:q`/`:Q` from
   a normal pager state always exit 0.
6. **Malformed near-ANSI escape sequences are silently dropped**, not caret-escaped, under `-R`.
   `ESC[2;5H` (valid mid-chars, invalid end-char `H`) disappears entirely rather than rendering as
   `^[` + literal text or similar. This is a narrow but exact-byte-parity-relevant rule if `-R`
   input ever contains non-color escape sequences.
7. **Line-number field width is a fixed 7 characters**, independent of the file's total line
   count or the current line's digit count, with exactly one separator space, and it repeats on
   every wrapped continuation row of a logical line — not just the first physical row.
8. **Two "window" units are not related by ÷2.** Full-window scroll (`Space`/`f`) moves by the
   content-row count (23 on an 80x24 pane); half-window scroll (`d`/`u`) moves by
   `ceil(full_screen_rows / 2)` = 12 (full 24-row screen ÷ 2), **not** `ceil(23/2)` = 12
   (coincidentally equal at this exact pane size — do not assume the relationship generalizes;
   derive half-window strictly from total rows, not from the content-row count).
9. **The 235/214 highlight color pair (`ESC[38;5;235m`/`ESC[48;5;214m`) is not a `ESC[7m`
   reverse-video toggle.** It is used consistently for: search-fail message, "Cannot seek…"
   error, "No next/previous file" boundary message, the initial-filename prompt segment, and
   search-match highlighting. Per the team's guidance, this color pair is treated as **a tmux
   rendering of the terminal's own standout/reverse-video capability**, not a literal SGR
   sequence less itself emits — see the reasoning note at the end of this section. `rust-less`
   should emit the terminal's standard standout/reverse-video escape (crossterm's
   `Attribute::Reverse`, i.e. `ESC[7m`) for all of the above content classes; on this same tmux
   version/terminfo, that is expected to render as the identical 235/214 pair, to be confirmed by
   an `.attrs.txt` comparison in Phase 5 (see §5). This is the one specification item resolved by
   inference rather than direct value-for-value citation, and is flagged as such intentionally.
10. **OSC 8 BEL-terminator representation is a capture-tooling caveat, not a confirmed less
    behavior.** The `ansi.txt` fixture includes one OSC 8 hyperlink terminated by literal BEL
    (`\x07`) in the source bytes; in the captured `.attrs.txt` output, the terminator appears
    indistinguishable from the `ESC\` form. This is plausibly a tmux `capture-pane -e`
    normalization of BEL-terminated OSC sequences rather than evidence that less rewrites the
    terminator. Phase 4 should pass through whichever terminator (`BEL` or `ESC\`) appeared in
    the source raw, unmodified; this specific sub-claim should **not** be used as a strict
    Phase 5 byte-comparison target beyond what the `compare_attrs: true` scenarios already lock
    in (see §5).
11. **Message text uniformly uses two spaces before a trailing parenthetical** across all
    transient status/error messages observed: `Pattern not found  (press RETURN)`,
    `Cannot seek to line number N  (press RETURN)`, `No next file  (press RETURN)`,
    `No previous file  (press RETURN)`, `"file" may be a binary file.  See it anyway?`. Treat
    this as a general formatting convention for any additional message text Phase 4 needs to
    emit for parity, not a per-message coincidence.
12. **Environment-color provenance caveat** (`docs/capture-findings.md`): the 235/214 pair is
    consistent enough across unrelated capture categories to be treated as less 668's genuine
    default on this system, but `CONTRACT.md`'s env scrub does not unset `LESS_TERMCAP_*`. If a
    future re-capture on a different machine yields different color codes for the same elements,
    check `LESS_TERMCAP_*`/`LESSCOLORS`/`LESS` before concluding less's behavior changed.

## §4 Architecture

Proposed module set (unchanged from the team's suggested decomposition — it maps cleanly
one-to-one onto the behavioral boundaries observed in §2/§3, so no restructuring is proposed):

```
cli ─┐
     ├─> command (main loop / state machine)
buffer ─┬─> screen ─┐
        │           ├─> command
        └─> search ─┘
                prompt ──> command
```

- **`cli`** (clap): parses argv into an `Options` struct — file list, `-N`/-`n`, `-S`, `-R`,
  `-m`/`-M`, `-q`/`-Q`, and (for stdin detection) whether any filename was given. No behavior
  logic; a pure argv → struct mapping. Depends on nothing else in the crate.
- **`buffer`**: owns incremental, per-file line access. Reads lazily (large-file startup speed,
  `startup-large-file`'s 5000-line fixture is the regression target — first screen must not block
  on a full read) and exposes line-count-so-far plus random access by line number, with a
  distinct "known EOF" state used by `nav-numeric-prefix`'s absolute-jump error path (§2/§3.3).
  Also owns raw-byte access per line for the UTF-8 (valid/malformed) and ANSI/control-char
  decoding done downstream in `screen`. Depends on nothing else.
- **`screen`**: pure rendering — turns a window of `buffer` lines plus `Options` (wrap/chop,
  `-N`, tab width 8, `-R` mode) into an 80×N (or resized W×H) cell grid with per-cell text +
  attribute. Owns: tab expansion, caret-notation/`LESSBINFMT` hex fallback, the `-S` chop-with-
  `>`-marker path, the default-wrap path (no continuation indicator), the `-R` ANSI/OSC8
  passthrough state machine (LESSANSIMIDCHARS/ENDCHARS, malformed-sequence silent-drop per §3.6),
  the binary-file detection that gates `-R`'s absence (§2 `opt-raw-control-chars-R` boundary), and
  `unicode-width`-based column accounting for wide/CJK glyphs (already a `Cargo.toml` dependency)
  used both for wrapping and for the line-number field's fixed 7-column width. Depends on
  `buffer` only (not on `command`) — it is a stateless line-window → grid function, which keeps
  the fresh-launch bottom-anchor quirk (§3.1) out of `screen` and in `command`, where the
  "first draw of this process" flag actually belongs conceptually (it's about *when* the window
  is positioned, which is a `command`/navigation concern, not a rendering concern).
- **`input`**: decodes crossterm key events into a logical `Action` (e.g. `ScrollLines(n)`,
  `GotoLine(n)`, `SearchForward(pattern)`, `NextFile(n)`, `Quit`) plus numeric-prefix
  accumulation (digits typed before a command char). Maps `Up`/`Down`/`PageUp`/`PageDown`
  directly to the same `Action` values as `k`/`j`/`b`/`f` (§2 `nav-arrow-pgup-pgdn-bindings`) —
  this is a **declared, accepted divergence** from real less's terminfo-driven binding mechanism:
  crossterm already decodes named keys platform-independently, so there is no terminfo lookup to
  replicate, and the observed behavioral *result* (arrow/PgUp/PgDn = alias of j/k/f/b) is what
  §2 requires, not the mechanism. Depends on nothing else in the crate (pure event → `Action`).
- **`search`**: regex-based (via the `regex` crate) forward/backward matching across `buffer`
  lines from a given start line, returning per-line match spans for `screen` to highlight (§2
  `search-highlight-matches`: **all** on-screen spans, not just the current one). Owns
  sticky last-pattern + last-direction state for `n`/`N` (§2 `search-repeat`/`search-repeat-
  reverse`). Depends on `buffer` and the `regex` crate.
- **`prompt`**: a small format-string interpreter for the three prompt templates quoted verbatim
  in §2 `prompt-short-colon`/`prompt-byte-percent-rules` (`?`/`%` escape codes: `%f`, `%i`, `%m`,
  `%T`, `%b`, `%p`/`%P`, `%l`/`%L`, `?n`/`?f`/`?m`/`?e`/`?x`/`?l`/`?p`/`?s` conditionals), plus
  the special-message override path (search-fail, cannot-seek, no-next/prev-file, binary-file-
  confirm — §3.11's two-space convention) and the left-truncation rule (§3.4) when the rendered
  text exceeds terminal width. Depends on `buffer` (line/byte counts, filename) and reads
  navigation/search state from `command` (current top line, file index, last-search-failed flag)
  rather than owning that state itself, since prompt content is derived from, not a source of,
  pager state.
- **`command`**: the main loop and state machine. Owns: current top-of-window line, sticky
  half-window N (§2 `nav-forward-halfwindow`), current file index and the multi-file list
  (`multifile-next-prev`), the "first draw of this process" flag (§3.1), and dispatches decoded
  `Action`s to `buffer`/`screen`/`search`/`prompt`. Handles terminal resize (crossterm resize
  event) by recomputing window height and re-invoking `screen` at the preserved top-of-window
  line (§2 `terminal-resize-redraw`: position preserved, not recentered) — the exact repaint
  mechanism (full clear vs. incremental) is an implementation choice not constrained by any
  baseline evidence (§3, item under `terminal-resize-redraw`; UNVERIFIED, see §5). Depends on
  `cli`, `buffer`, `screen`, `input`, `search`, `prompt` — this is the only module allowed to
  depend on all the others, keeping the dependency graph acyclic.

## §5 Parity Acceptance Criteria

Ties `parity/harness/CONTRACT.md`'s comparison rules to the feature groups in §2.

**Visual+Behavioral (`.txt` full match, required for all 35 `feature_id`s):** every scenario's
plain `capture-pane -p` text must match byte-for-byte after canonicalization. This is the primary
gate for §2's startup, navigation, search (text, not highlight), display-option, multi-file,
prompt-text, and resize-endstate behaviors — i.e. everything in §2 except the attrs-specific
claims below.

**Attrs (`.attrs.txt`) comparison — scoped, per `CONTRACT.md`, to scenarios with
`"compare_attrs": true` only.** Exactly four scenarios in `parity/scenarios/` carry this flag:
`search-forward-basic`, `search-forward-multi-match-repeat`, `opt-raw-control-chars`, and
`prompt-initial-filename`. These map to feature groups: `search-highlight-matches`/`search-
forward`/`search-repeat`/`search-repeat-reverse` (highlight span attrs), `opt-raw-control-chars-R`/
`ansi-escape-handling` (color passthrough attrs), and `prompt-initial-filename`/`prompt-short-
colon` (filename-highlight vs. plain-colon-prompt zero-attrs distinction). No other feature_id's
attrs are a Phase 5 gate — e.g. `search-not-found-message`'s 235/214 color is documented in §2 but
its scenario (`search-not-found`) does not set `compare_attrs`, so only its `.txt` text is a hard
gate; matching attrs there is expected (§3.9's standout-emission strategy) but not independently
diffed.

**Exit codes:** exact match required for every scenario's `exit_code` file, including the two
exit-1 paths in §2 `exit-codes` (file-not-found, binary-file-declined) alongside the five exit-0
quit forms.

**Feature coverage:** `matrix.json` must show all 35 `feature_id`s covered by ≥1 scenario — already
satisfied by the existing 39 scenarios (verified in this document's §2, one subsection per
`feature_id`, each citing ≥1 scenario).

**Declared allowable differences (parity ceilings, §1):** regex-dialect divergence outside the two
tested patterns; crossterm init-sequence differences (currently moot, not part of `.txt`/`.attrs`
comparison).

**UNVERIFIED — not Phase 5 diff targets, implement per man-page/ledger claim only:**
- Bell audibility/visual-flash (§1; `prompt-error-bell` feature — screen state is a gate, bell
  itself is not).
- Resize repaint *mechanism/timing* (§2 `terminal-resize-redraw`; end-state top-of-window
  preservation **is** a gate, the byte-level redraw transcript is not).
- OSC 8 BEL-vs-`ESC\` terminator representation (§3, item 10) beyond what the `opt-raw-control-
  chars` `compare_attrs: true` scenario already locks in.
- `-K`/`^C` → exit code 2 (`unconfirmed-lead`, no scenario), `-G`/suppress-highlight
  (`unconfirmed-lead`), the `=` file-info message (`unconfirmed-lead`), `z`/`w` window-resize-
  and-scroll commands (`unconfirmed-lead`), `LESS_LINES`/`LESS_COLUMNS`/`-j`/`-#` fractional
  resize recalculation (`unconfirmed-lead`) — all per `docs/evidence-ledger.json`'s residual
  `unconfirmed-lead` entries and `docs/capture-findings.md`'s "Residual gaps" section. Implement
  per the man-page claim cited in the ledger for completeness, but a Phase 5 mismatch on these
  specific behaviors should not block ship — there is no baseline oracle value to diff against.
- Wrap-line × window-boundary scroll behavior: the implementation accumulates full logical
  lines and never splits a wrapped line across a window boundary (src/screen.rs
  `render_window`). No baseline scenario exercises a wrapped line straddling the boundary;
  design choice, not capture-derived.
- `:n`/`:p` to a nonexistent file (defensive path: pending message shown, current file kept):
  no baseline scenario; guard implementation, not capture-derived.
