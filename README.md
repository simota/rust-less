# rless

A terminal pager for logs and data, in Rust. Its core is a clean-room clone of
GNU less 668 (checked byte for byte against captured less screens with
`make parity`); on top of that it reads JSON lines, CSV, TSV and logfmt as
tables you can filter, sort and summarise, and it follows growing files and
pipes.

```sh
make release            # target/release/rless
make install            # ~/.cargo/bin/rless
```

## Everyday use

```sh
rless app.log                     # page a file (or several: :n / :p)
kubectl logs -f api | rless       # page a pipe; F follows it
rless +F /var/log/app.log         # start following (like tail -f)
rless -p 'request 42' app.log     # start at the first match (+/pat, +G, +120 too)
rless -iS app.log                 # smart-case search, chop long lines
git config --global core.pager 'rless -FRX'   # rless as git's pager
```

When stdout is not a terminal, rless copies its input through like `cat`.
Flags it understands are also taken from `$LESS` (others are ignored).

## Narrowing a log down

| Keys | What it does |
|---|---|
| `/pat` `?pat` `n` `N` | search forward / backward, repeat |
| `&ERROR` | show only matching lines (any file) |
| `&!DEBUG` | hide matching lines |
| `&` + Enter | clear the filter |
| `F` | follow: keep reading as the input grows (any key stops); a filter stays live |
| `:w file` / `:w! file` | save what the view shows (filtered, sorted) to a file |

While a filter or sort is active the prompt says so, e.g. `[&ERROR, 42 lines]`,
and every movement key moves over the lines shown, never hidden ones.

## Structured logs and tables

The format is detected from the first lines (`s` cycles it by hand).

| Keys | What it does |
|---|---|
| `c` | table view; `H` sticky header; `z` expand a JSON line |
| `x` | column mode: `h`/`l` move, `p` pin, `.` hide, `s` sort (asc, desc, off), `i` statistics |
| `Right` `Left` | scroll a wide table (or a `-S` view) sideways |

Selector queries work in `/` and `&` on structured files:

```text
&.status >= 500
&.level == "ERROR" && .service != "auth"
&.ms > 250ms || .dur >= 1.5s
&.user.id == 7                 nested JSON
&!.trace_id                    field missing (a bare path tests that it exists)
&.msg =~ "timeout|refused"     regex on the value
&(.level == "WARN" || .level == "ERROR") && .ts >= "2026-08-23T00:30"
```

Anything that is not a selector is a regular expression, as in less.

## More keys

`m a` / `' a` set and jump to a mark, `''` returns to where the last jump
started; `-N` `-S` `-i` `-I` toggle options inside the pager; `h` shows every
key. `rless --help` lists the options and keys.

## Development

```sh
make check      # fmt, clippy -D warnings, tests, scenario validation
make parity     # 40 screen scenarios against captured GNU less output (needs tmux)
make probe      # scripted end-to-end tasks, docs/fvi (needs tmux)
make bench      # opt-in hot-path benchmarks
```

Design notes, the parity contract and quality reports live in `docs/`.
