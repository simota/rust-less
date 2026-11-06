# rless

A clean-room Rust pager. Its core reproduces GNU less 668 cell for cell
(40/40 captured parity scenarios, `make parity`); on top of that it reads
structured logs and tables (JSON lines, CSV, TSV, logfmt) as tables you can
filter, search by field, sort and scroll sideways.

```sh
make release            # target/release/rless
make install            # into ~/.cargo/bin
```

## Everyday use

```sh
rless app.log                       # page a file
kubectl logs -f api | rless         # page a pipe; press F to follow it live
rless +G app.log                    # start at the end (+/err, +150 work too)
rless -p 'panic|fatal' app.log      # start at the first match
rless -i app.log                    # searches ignore case unless they have capitals
rless --filter '.status >= 500' access.jsonl   # open already filtered
git log | rless -FX                 # print and exit if it fits one screen
```

Press `h` inside the pager for the full key list. The keys less users
expect work as in less: `j k f b d u g G / ? n N :n :p q`.

| Task | Keys |
|---|---|
| Show only matching lines / hide matching lines / clear | `&pattern` / `&!pattern` / `&` |
| Follow a growing file or a still-open pipe | `F` (any key stops) |
| Set a mark / go back to it / back to before the last jump | `ma` / `'a` / `''` |
| Jump to a percentage | `50%` or `50p` |
| Scroll long lines sideways (best with `-S`) | `Right` / `Left`, `ESC-{` / `ESC-}` |
| Toggle line numbers, chopping, case-insensitive search | `-N`, `-S`, `-i`, `-I` |
| Where am I (lines, bytes, %, filter, sort) | `=` or `^G` |
| Re-read a file that changed / open it in `$EDITOR` | `R` / `v` |
| Open another file / go to the Nth file | `:e path` / `N:x` |

Each file remembers its own position, filter and table settings when you
switch away with `:n`/`:p` and come back.

## Structured input

JSON lines, CSV, TSV and logfmt are detected from the first lines (`s`
cycles the format if detection guessed wrong).

| Task | Keys |
|---|---|
| Table view / sticky header row | `c` / `H` |
| Expand the JSON line at the top | `z` |
| Column mode: move, pin, hide, sort asc → desc → off | `x`, then `h l`, `p`, `.`, `s`; `q` leaves |
| Search or filter by field | `/.status >= 500`, `&.level == "ERROR"`, `&.ms > 250ms` |
| Combine conditions | `&.level == "ERROR" && .ms > 250 \|\| .status >= 500` |
| Nested JSON fields / CSV columns by header name | `&.user.id == 42` / `&.status >= 500` |

Operators: `== != > >= < <= =~` (`=~` is "contains"). Durations (`250ms`,
`2s`, `1m`, `1h`) compare as milliseconds. A pinned column stays in place
while the rest of the table scrolls sideways. Sorting builds a sorted
snapshot of the whole input (a CSV header row stays on top); `F` and `R`
return to file order.

## Verification

* `make check` — rustfmt, clippy (warnings are errors), unit and
  integration tests, scenario validation.
* `make parity` — replays 40 scenarios in tmux and diffs every captured
  cell grid against the GNU less 668 baseline. This is the gate for the
  less-compatible core and must stay 40/40.
* The features beyond that core (everything in the tables above that less
  also has, and all structured views) have no less baseline in
  `parity/baseline/`; they are covered by tests in `src/` and were checked
  by hand in tmux. See `docs/RECONSTRUCTION_SPEC.md` for the parity scope.
