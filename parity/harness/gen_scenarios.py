#!/usr/bin/env python3
"""Generate parity/scenarios/*.json deterministically from the data table
below. Analogous to gen_fixtures.py: scenarios are committed as generated
artifacts so the hub can run.sh directly against them, and re-running this
script regenerates byte-identical files.

See parity/harness/CONTRACT.md for the base scenario schema and
parity/harness/EXTENSIONS.md for the small set of additive fields used here
(extra_fixtures, capture-only/resize steps, compare_attrs, expect_fixture_missing).
"""
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
SCENARIOS_DIR = os.path.normpath(os.path.join(HERE, "..", "scenarios"))
FX = "parity/fixtures/"

# NOTE: scenario ids double as filenames (<id>.json) and capture directory
# names. Do not introduce two ids that differ only by case (e.g. "exit-q" /
# "exit-Q") - macOS's default APFS is case-insensitive and one file would
# silently clobber the other. validate_scenarios.py checks for this.
SCENARIOS = [
    # --- startup ---------------------------------------------------------
    {
        "id": "startup-single-file",
        "feature_ids": ["startup-file-arg"],
        "fixture": FX + "short.txt",
        "args": [],
        "stdin": False,
        "steps": [{"capture": "first-screen"}],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "startup-large-file",
        "feature_ids": ["startup-file-arg"],
        "fixture": FX + "large.txt",
        "args": [],
        "stdin": False,
        "steps": [{"capture": "first-screen"}],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "startup-stdin-pipe",
        "feature_ids": ["startup-stdin-pipe"],
        "fixture": FX + "short.txt",
        "args": [],
        "stdin": True,
        "steps": [{"capture": "first-screen"}],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "startup-multi-file",
        "feature_ids": ["startup-multi-file-args", "multifile-prompt-indicator", "multifile-next-prev"],
        "fixture": FX + "multi-1.txt",
        "extra_fixtures": [FX + "multi-2.txt", FX + "multi-3.txt"],
        "args": [],
        "stdin": False,
        "steps": [
            {"capture": "file1-initial"},
            {"keys": ":n", "capture": "after-colon-n-to-file2"},
            {"keys": ":n", "capture": "after-colon-n-to-file3"},
            {"keys": ":n", "capture": "after-colon-n-at-last-boundary"},
            {"keys": ":p", "capture": "after-colon-p-back-to-file2"},
            {"keys": ":p", "capture": "after-colon-p-back-to-file1"},
            {"keys": ":p", "capture": "after-colon-p-at-first-boundary"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "multi-file-count-nav",
        "feature_ids": ["multifile-next-prev"],
        "fixture": FX + "multi-1.txt",
        "extra_fixtures": [FX + "multi-2.txt", FX + "multi-3.txt"],
        "args": [],
        "stdin": False,
        "steps": [
            {"capture": "initial-file1"},
            {"keys": "2:n", "capture": "after-2colon-n-to-file3"},
            {"keys": "2:p", "capture": "after-2colon-p-to-file1"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    # --- navigation --------------------------------------------------------
    {
        "id": "nav-line-forward-backward",
        "feature_ids": ["nav-forward-line", "nav-backward-line"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "steps": [
            {"capture": "initial"},
            {"keys": "jjj", "capture": "after-3-single-j"},
            {"keys": "kk", "capture": "after-2-single-k"},
            {"key": "Enter", "capture": "after-enter-forward-1"},
            {"keys": "5j", "capture": "after-5j"},
            {"keys": "3k", "capture": "after-3k"},
            {"keys": "50j", "capture": "after-50j-exceeds-window-height"},
            {"keys": "20k", "capture": "after-20k"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "nav-boundary-eof-bof",
        "feature_ids": ["nav-forward-line", "nav-backward-line", "prompt-end-marker"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "steps": [
            {"capture": "initial"},
            {"keys": "k", "capture": "k-at-bof-noop"},
            {"keys": "300j", "capture": "after-300j-clamped-to-eof"},
            {"keys": "j", "capture": "j-at-eof-noop"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "nav-window-forward-backward",
        "feature_ids": ["nav-forward-window", "nav-backward-window"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "steps": [
            {"capture": "initial"},
            {"key": "Space", "capture": "after-space-fwd-1window"},
            {"key": "Space", "capture": "after-space-fwd-2window"},
            {"keys": "b", "capture": "after-b-back-1window"},
            {"keys": "10f", "capture": "after-10f-lt-window"},
            {"keys": "50f", "capture": "after-50f-gt-window"},
            {"keys": "10b", "capture": "after-10b-lt-window"},
            {"keys": "50b", "capture": "after-50b-gt-window"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "nav-halfwindow",
        "feature_ids": ["nav-forward-halfwindow", "nav-backward-halfwindow"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "steps": [
            {"capture": "initial"},
            {"keys": "d", "capture": "after-d-default-half"},
            {"keys": "d", "capture": "after-d-again"},
            {"keys": "u", "capture": "after-u-back-half"},
            {"keys": "5d", "capture": "after-5d-sets-new-sticky-default"},
            {"keys": "d", "capture": "after-bare-d-uses-sticky-5"},
            {"keys": "u", "capture": "after-bare-u-uses-sticky-5"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "nav-goto-line",
        "feature_ids": ["nav-goto-first-line", "nav-goto-last-line", "nav-numeric-prefix"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "steps": [
            {"capture": "initial"},
            {"keys": "G", "capture": "after-G-goto-last"},
            {"keys": "g", "capture": "after-g-goto-first"},
            {"keys": "10g", "capture": "after-10g-mid-file"},
            {"keys": "100g", "capture": "after-100g-mid-file"},
            {"keys": "9999g", "capture": "after-9999g-beyond-eof-clamped"},
            {"keys": "50G", "capture": "after-50G-mid-file"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "nav-numeric-prefix-edge",
        "feature_ids": ["nav-numeric-prefix"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "steps": [
            {"capture": "initial"},
            {"keys": "0g", "capture": "after-0g-zero-edge-case"},
            {"keys": "999999999999g", "capture": "after-huge-number-g"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "nav-arrow-pgup-pgdn",
        "feature_ids": ["nav-arrow-pgup-pgdn-bindings"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "steps": [
            {"keys": "50g", "capture": "initial-at-line-50"},
            {"key": "Down", "capture": "after-down-arrow"},
            {"key": "Up", "capture": "after-up-arrow"},
            {"key": "NPage", "capture": "after-pagedown"},
            {"key": "PPage", "capture": "after-pageup"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    # --- search --------------------------------------------------------
    {
        "id": "search-forward-basic",
        "feature_ids": ["search-forward", "search-highlight-matches"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "compare_attrs": True,
        "steps": [
            {"capture": "initial"},
            {"keys": "/SOLO_NEEDLE", "key_after": "Enter", "capture": "after-search-single-match"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "search-forward-multi-match-repeat",
        "feature_ids": ["search-forward", "search-repeat", "search-repeat-reverse", "search-highlight-matches"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "compare_attrs": True,
        "steps": [
            {"capture": "initial"},
            {"keys": "/ALPHA_TOKEN", "key_after": "Enter", "capture": "after-first-match"},
            {"keys": "n", "capture": "after-n-second-match"},
            {"keys": "n", "capture": "after-n-third-match"},
            {"keys": "N", "capture": "after-N-reverse-back-to-second"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "search-backward-basic",
        "feature_ids": ["search-backward"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "steps": [
            {"keys": "G", "capture": "initial-at-eof"},
            {"keys": "?SOLO_NEEDLE", "key_after": "Enter", "capture": "after-backward-search"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "search-not-found",
        "feature_ids": ["search-not-found-message"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "steps": [
            {"capture": "initial"},
            {"keys": "/NOPE_NOT_PRESENT_XYZ", "key_after": "Enter", "capture": "after-forward-search-fail"},
            {"keys": "?NOPE_NOT_PRESENT_XYZ", "key_after": "Enter", "capture": "after-backward-search-fail"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "search-regex-basic",
        "feature_ids": ["search-forward"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "steps": [
            {"capture": "initial"},
            {"keys": "/[cb]at", "key_after": "Enter", "capture": "after-charclass-regex"},
            {"keys": "/foo.*456", "key_after": "Enter", "capture": "after-dotstar-regex"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    # --- display options --------------------------------------------------------
    {
        "id": "opt-line-numbers",
        "feature_ids": ["opt-line-numbers-N"],
        "fixture": FX + "lines200.txt",
        "args": ["-N"],
        "stdin": False,
        "steps": [
            {"capture": "initial-with-line-numbers"},
            {"keys": "50g", "capture": "mid-file-with-line-numbers"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "opt-line-numbers-longline-interaction",
        "feature_ids": ["opt-line-numbers-N"],
        "fixture": FX + "longlines.txt",
        "args": ["-N"],
        "stdin": False,
        "steps": [{"capture": "initial-longline-with-numbers"}],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "opt-chop-long-lines",
        "feature_ids": ["opt-chop-long-lines-S", "default-display-formatting"],
        "fixture": FX + "longlines.txt",
        "args": ["-S"],
        "stdin": False,
        "steps": [{"capture": "chopped-view"}],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "opt-wrap-default",
        "feature_ids": ["default-display-formatting"],
        "fixture": FX + "longlines.txt",
        "args": [],
        "stdin": False,
        "steps": [{"capture": "wrapped-view"}],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "opt-raw-control-chars",
        "feature_ids": ["opt-raw-control-chars-R", "ansi-escape-handling"],
        "fixture": FX + "ansi.txt",
        "args": ["-R"],
        "stdin": False,
        "compare_attrs": True,
        "steps": [{"capture": "ansi-raw-view"}],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "opt-raw-control-chars-without-R",
        "feature_ids": ["opt-raw-control-chars-R"],
        "fixture": FX + "ansi.txt",
        "args": [],
        "stdin": False,
        "steps": [{"capture": "ansi-without-R-view"}],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "default-tabs-and-control-chars",
        "feature_ids": ["default-display-formatting"],
        "fixture": FX + "tabs-ctrl.txt",
        "args": [],
        "stdin": False,
        "steps": [{"capture": "tabs-and-ctrl-view"}],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "utf8-handling",
        "feature_ids": ["utf8-handling"],
        "fixture": FX + "utf8-mixed.txt",
        "args": [],
        "stdin": False,
        "steps": [{"capture": "utf8-view"}],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "utf8-malformed",
        "feature_ids": ["utf8-handling"],
        "fixture": FX + "utf8-malformed.txt",
        "args": [],
        "stdin": False,
        "steps": [{"capture": "malformed-utf8-view"}],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    # --- prompts --------------------------------------------------------
    {
        "id": "prompt-initial-filename",
        "feature_ids": ["prompt-initial-filename", "prompt-short-colon"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "compare_attrs": True,
        "steps": [
            {"capture": "first-prompt-with-filename"},
            {"keys": "j", "capture": "second-prompt-no-filename"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "prompt-medium-M",
        "feature_ids": ["prompt-byte-percent-rules"],
        "fixture": FX + "lines200.txt",
        "args": ["-m"],
        "stdin": False,
        "steps": [
            {"capture": "initial-medium-prompt"},
            {"keys": "50g", "capture": "mid-file-medium-prompt"},
            {"keys": "G", "capture": "eof-medium-prompt"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "prompt-long-M",
        "feature_ids": ["prompt-byte-percent-rules"],
        "fixture": FX + "lines200.txt",
        "args": ["-M"],
        "stdin": False,
        "steps": [
            {"capture": "initial-long-prompt"},
            {"keys": "50g", "capture": "mid-file-long-prompt"},
            {"keys": "G", "capture": "eof-long-prompt"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "prompt-error-bell-default",
        "feature_ids": ["prompt-error-bell"],
        "fixture": FX + "short.txt",
        "args": [],
        "stdin": False,
        "steps": [
            {"capture": "initial-already-at-eof"},
            {"keys": "j", "capture": "after-j-past-eof"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "prompt-error-bell-partial-quiet",
        "feature_ids": ["prompt-error-bell"],
        "fixture": FX + "short.txt",
        "args": ["-q"],
        "stdin": False,
        "steps": [
            {"capture": "initial-already-at-eof"},
            {"keys": "j", "capture": "after-j-past-eof"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "prompt-error-bell-full-quiet",
        "feature_ids": ["prompt-error-bell"],
        "fixture": FX + "short.txt",
        "args": ["-Q"],
        "stdin": False,
        "steps": [
            {"capture": "initial-already-at-eof"},
            {"keys": "j", "capture": "after-j-past-eof"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    # --- exit commands / codes (capture-determined) --------------------------------------------------------
    {
        "id": "exit-lower-q",
        "feature_ids": ["exit-commands", "exit-codes"],
        "fixture": FX + "short.txt",
        "args": [],
        "stdin": False,
        "steps": [{"capture": "before-quit"}],
        "quit_keys": "q",
        "expect_exit": 0,
    },
    {
        "id": "exit-upper-q",
        "feature_ids": ["exit-commands", "exit-codes"],
        "fixture": FX + "short.txt",
        "args": [],
        "stdin": False,
        "steps": [{"capture": "before-quit"}],
        "quit_keys": "Q",
        "expect_exit": 0,
    },
    {
        "id": "exit-colon-lower-q",
        "feature_ids": ["exit-commands", "exit-codes"],
        "fixture": FX + "short.txt",
        "args": [],
        "stdin": False,
        "steps": [{"capture": "before-quit"}],
        "quit_keys": ":q",
        "expect_exit": 0,
    },
    {
        "id": "exit-colon-upper-q",
        "feature_ids": ["exit-commands", "exit-codes"],
        "fixture": FX + "short.txt",
        "args": [],
        "stdin": False,
        "steps": [{"capture": "before-quit"}],
        "quit_keys": ":Q",
        "expect_exit": 0,
    },
    {
        "id": "exit-ZZ",
        "feature_ids": ["exit-commands", "exit-codes"],
        "fixture": FX + "short.txt",
        "args": [],
        "stdin": False,
        "steps": [{"capture": "before-quit"}],
        "quit_keys": "ZZ",
        "expect_exit": 0,
    },
    {
        "id": "exit-code-file-not-found",
        "feature_ids": ["exit-codes"],
        "fixture": FX + "does-not-exist.txt",
        "expect_fixture_missing": True,
        "args": [],
        "stdin": False,
        "steps": [{"capture": "error-view"}],
        "quit_keys": "q",
        "expect_exit": None,
    },
    # --- resize (capture-determined) --------------------------------------------------------
    {
        "id": "terminal-resize-redraw",
        "feature_ids": ["terminal-resize-redraw"],
        "fixture": FX + "lines200.txt",
        "args": [],
        "stdin": False,
        "steps": [
            {"keys": "50g", "capture": "before-resize-mid-file"},
            {"resize": {"cols": 100, "rows": 30}, "capture": "after-resize-100x30"},
            {"keys": "j", "capture": "after-resize-then-scroll"},
        ],
        "quit_keys": "q",
        "expect_exit": 0,
    },
]


def main() -> None:
    os.makedirs(SCENARIOS_DIR, exist_ok=True)
    seen_ids = set()
    for scenario in SCENARIOS:
        sid = scenario["id"]
        if sid in seen_ids:
            raise SystemExit(f"duplicate scenario id in table: {sid}")
        seen_ids.add(sid)
        path = os.path.join(SCENARIOS_DIR, f"{sid}.json")
        with open(path, "w", encoding="utf-8") as f:
            json.dump(scenario, f, indent=2)
            f.write("\n")
        print(f"wrote {path}")
    print(f"total: {len(SCENARIOS)} scenario file(s)")


if __name__ == "__main__":
    main()
