# Runner extensions beyond CONTRACT.md (proposal, hub review requested)

CONTRACT.md's scenario schema and runner IF are followed as written. The
points below are additive interpretations needed to cover cases the base
schema/IF are silent on (multi-file args, resize steps, capture-only steps).
None of them change the meaning of an existing field; all are optional. If
the hub prefers different names/shapes, only gen_scenarios.py and run.py
need to change - the scenario JSON is regenerated, not hand-edited.

1. **`extra_fixtures` (array, optional)** - CONTRACT.md's `fixture` field is
   singular, but multi-file scenarios (`startup-multi-file-args`,
   `multifile-next-prev`, `multifile-prompt-indicator`) need >1 filename
   argument. `extra_fixtures` paths are appended after `fixture` (and after
   `args`) when building the non-stdin argv, in array order. Unused for
   `stdin: true` scenarios (only `fixture` - the piped file - applies there).

2. **Capture-only steps** - a step with only `"capture": "name"` (no
   `keys`/`key`) is valid: it waits `wait_ms` then captures, with no key
   sent. Used for "first screen, no interaction yet" captures (e.g.
   `startup-single-file`'s `first-screen`).

3. **`{"resize": {"cols": N, "rows": N}, "capture": "..."}` step** - only
   used by `terminal-resize-redraw.json` per the 80x24 -> 100x30 constraint.
   Runs `tmux resize-window -x N -y N` before the step's normal wait/capture.

4. **`initial_size` (array `[cols, rows]`, scenario-level, optional)** -
   overrides the 80x24 default pane size at session creation. Unused by any
   scenario in this set (all start at 80x24 and only `terminal-resize-redraw`
   changes size mid-session via a resize step) but supported by run.py in
   case a future scenario needs a non-default starting size.

5. **`wait_ms` (scenario-level and/or per-step, optional)** - overrides the
   400ms default settle wait. Unused in this set; included for future
   scenarios against slower candidates.

6. **`compare_attrs` scope** - CONTRACT.md ties `compare_attrs: true` to "the
   scenario" without specifying per-capture granularity. This runner treats
   it as scenario-wide: when true, diff.sh compares `.attrs.txt` for every
   capture in that scenario, not a subset. Scenarios that only care about
   attrs on one specific capture (e.g. `prompt-initial-filename`, which has
   two captures) still get both captures' attrs compared; this is stricter
   than necessary but never wrong (attrs equality is still a legitimate
   requirement even for the "second-prompt" capture there).

7. **Uniform exit-code wrapper** - CONTRACT.md specifies the `sh -c 'cat
   FIXTURE | BINARY'` wrapper + `echo $? > exit_code` pattern for stdin
   scenarios specifically. run.py uses the same wrapper (`<cmd>; echo $? >
   exit_code`, wrapped in `sh -c '...'`) uniformly for non-stdin scenarios
   too, since it is simpler to implement once and is observationally
   identical (capture-pane output is unaffected by how the process was
   launched). The exit code is read from a real file on disk, not via tmux's
   `pane_dead_status`, so it survives even if the tmux session/pane has
   already torn down by the time we poll (e.g. an immediate-exit error path).

8. **`meta.json` per scenario output dir** - run.py writes
   `<outdir>/<id>/meta.json` (`{feature_ids, compare_attrs, expect_exit}`)
   alongside the CONTRACT-specified `.txt`/`.attrs.txt`/`exit_code`/
   `steps.log`. This lets diff.sh operate on two bare output directories
   without also needing access to `parity/scenarios/` - useful if
   baseline/candidate captures are archived separately from the scenario
   source. Not in CONTRACT.md's list of per-scenario outputs; purely
   additive.

9. **`diff.sh` report location** - CONTRACT.md names `matrix.json` as an
   artifact but not its path. This runner writes
   `<candidate-dir>/_diff-report/matrix.json` and per-mismatch unified diffs
   under `<candidate-dir>/_diff-report/<scenario-id>/<capture>.txt[.attrs].diff`
   (only written for FAILing captures, to keep the report lean). The `_`
   prefix keeps it visually distinct from scenario-id subdirectories (no
   scenario id in this set starts with `_`).

10. **`expect_fixture_missing` (bool, scenario-level, optional)** - lets
    `exit-code-file-not-found.json` reference a deliberately nonexistent
    fixture path without failing `validate-scenarios.sh`'s on-disk
    existence check.

11. **`expect_exit: null`** - used where the man page gives no prior to
    anchor a guess (only `exit-code-file-not-found.json`). Everywhere else
    `expect_exit` carries the ledger's provisional assumption (0 for normal
    quit) - but per CONTRACT.md and the evidence ledger, the *authoritative*
    value for every capture-determined item comes from the baseline capture
    itself, not from this field. diff.sh does not read `expect_exit` at all;
    it only compares baseline's actual exit_code against candidate's.

## Scenario id / filename collision note (fixed, not a proposal)

Scenario ids double as filenames and capture-directory names. Six ids
originally differed only by case (`exit-q`/`exit-Q`,
`exit-colon-q`/`exit-colon-Q`, `prompt-error-bell-q`/`prompt-error-bell-Q`),
which silently collided as files on case-insensitive filesystems (default
macOS APFS - the target platform per provenance.json) and dropped 3 of 39
scenarios with no error. Renamed to case-distinct ids (`exit-lower-q`/
`exit-upper-q`, `exit-colon-lower-q`/`exit-colon-upper-q`,
`prompt-error-bell-partial-quiet`/`prompt-error-bell-full-quiet`).
`validate_scenarios.py` now flags any future case-only collision.
