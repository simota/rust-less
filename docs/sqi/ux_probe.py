#!/usr/bin/env python3
"""SQI scripted task probe for rless (docs/sqi/SQI_REPORT.md, UXQI 7.1).

Drives the release binary in a fixed-size tmux pane (80x24 unless a task
says otherwise, LANG=C.UTF-8), replays a fixed key sequence per task, and
checks one success condition on the captured screen. The task set and the
success conditions were fixed BEFORE any improvement work, one task per
user-visible feature (core pager + every advertised structured-log feature);
they are not edited afterwards.

This is a scripted functional check run by an AI agent, NOT a user test:
it measures whether a task *can* be completed with the documented keys, not
how long a person takes or whether they discover the keys.

usage: python3 docs/sqi/ux_probe.py [path/to/rless]   (needs tmux; unsandboxed)
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

SOCK = "rless-sqi-probe"


def tmux(*args, check=True):
    return subprocess.run(["tmux", "-L", SOCK, *args], check=check,
                          capture_output=True, text=True).stdout


def run_task(binary, args, keys, width=80, height=24):
    """Launches `binary args`, sends each element of `keys` (tmux key names,
    or ("lit", text) for literal text) and returns the final screen lines."""
    tmux("kill-server", check=False)
    cmd = "env -u LESS -u LESSOPEN -u LESSCLOSE LESSHISTFILE=- LANG=C.UTF-8 " \
          f"{binary} {args}"
    tmux("new-session", "-d", "-x", str(width), "-y", str(height), cmd)
    time.sleep(0.5)
    for k in keys:
        if isinstance(k, tuple):
            tmux("send-keys", "-l", k[1])
        else:
            tmux("send-keys", k)
        time.sleep(0.25)
    screen = tmux("capture-pane", "-p").split("\n")
    tmux("kill-server", check=False)
    return screen[:height]


def make_fixtures(d):
    levels = ["INFO", "DEBUG", "ERROR", "WARN"]
    services = ["auth", "cart", "payment", "gateway"]
    with open(os.path.join(d, "logs.jsonl"), "w") as f:
        for i in range(1, 61):
            f.write(json.dumps({
                "ts": f"2026-08-23T00:{i:02d}:00Z", "level": levels[i % 4],
                "status": [200, 200, 500, 404][i % 4],
                "service": services[i % 4], "latency_ms": i * 7,
                "msg": f"request {i} handled"}, separators=(",", ":")) + "\n")
    with open(os.path.join(d, "plain.txt"), "w") as f:
        for i in range(1, 61):
            f.write(f"line {i} .status >= 500 literal text\n")
    with open(os.path.join(d, "people.csv"), "w") as f:
        f.write("name,age,city\nalice,30,tokyo\nbob,25,osaka\n")
    with open(os.path.join(d, "ctrl.jsonl"), "wb") as f:
        f.write(b'{"level":"INFO","msg":"bell\x07here"}\n'
                b'{"level":"WARN","msg":"cr\rXX"}\n'
                b'{"level":"INFO","msg":"ok"}\n')


def L(text):
    return ("lit", text)


def body(screen):
    return screen[:-1]


def prompt(screen):
    return screen[-1]


# (id, description, args, keys, success predicate, optional (w, h))
TASKS = [
    ("T01", "open a plain file and jump to line 50", "plain.txt",
     [L("50g")], lambda s: s[0].startswith("line 50 ")),
    ("T02", "regex search in a plain file", "plain.txt",
     [L("/line 3"), "Enter"], lambda s: s[0].startswith("line 3 ")),
    ("T03", "repeat a plain search with n", "plain.txt",
     [L("/line 3"), "Enter", "n"], lambda s: s[0].startswith("line 30 ")),
    ("T04", "search a plain file for literal text that looks like a "
     "selector ('.status >= 500')", "plain.txt",
     ["j", L("/.status >= 500"), "Enter"],
     lambda s: "not found" not in prompt(s) and s[0].startswith("line ")),
    ("T05", "switch to the tabular view of JSON lines", "logs.jsonl",
     ["c"], lambda s: "│" in s[0] and "DEBUG" in s[0]),
    ("T06", "structured search .status >= 500", "logs.jsonl",
     ["c", L("/.status >= 500"), "Enter"], lambda s: "│500" in s[0]),
    ("T07", "repeat a structured search with n", "logs.jsonl",
     ["c", L("/.status >= 500"), "Enter", "n"],
     lambda s: "request 6 handled" in s[0]),
    ("T08", "structured filter keeps only matching rows", "logs.jsonl",
     ["c", L('&.level == "ERROR"'), "Enter"],
     lambda s: all("ERROR" in r for r in body(s) if "│" in r)
     and sum("│" in r for r in body(s)) >= 10),
    ("T09", "invalid filter pattern is reported, not silently applied",
     "logs.jsonl", ["c", L("&(unclosed"), "Enter"],
     lambda s: "invalid" in prompt(s).lower()),
    ("T10", "tabular view of a CSV file shows its own columns, untruncated",
     "people.csv", ["c"],
     lambda s: "alice" in s[1] and "tokyo" in s[1]
     and "│     │" not in s[1]),
    ("T11", "tabular view of the 2nd file (CSV after JSONL)",
     "logs.jsonl people.csv", [L(":n"), "c"],
     lambda s: any("alice" in r and "│" in r for r in body(s))),
    ("T12", "narrow terminal (50x10): table rows do not spill into the "
     "prompt row", "logs.jsonl", ["c", "j"],
     lambda s: "│" not in prompt(s) and "handled" not in prompt(s),
     (50, 10)),
    ("T13", "sticky header stays aligned with data after pinning a column",
     "logs.jsonl", ["c", "H", "x", "l", "l", "p", "q"],
     lambda s: s[0].startswith("status") and s[1].startswith("200")),
    ("T14", "column mode tells which column is focused (by name)",
     "logs.jsonl", ["c", "x", "l", "l"],
     lambda s: "status" in prompt(s)),
    ("T15", "column mode cannot move focus past the last column",
     "logs.jsonl", ["c", "x"] + ["l"] * 9,
     lambda s: "msg" in prompt(s) and "9" not in prompt(s)),
    ("T16", "pinning a column gives visible confirmation", "logs.jsonl",
     ["c", "x", "p"], lambda s: "pinned" in prompt(s).lower()),
    ("T17", "control characters in a field are shown escaped, not executed",
     "ctrl.jsonl", ["c"],
     lambda s: "^G" in s[0] and "^M" in s[1] and s[1].startswith("WARN")),
]


def main():
    binary = os.path.abspath(sys.argv[1] if len(sys.argv) > 1
                             else "target/release/rless")
    if not shutil.which("tmux"):
        sys.exit("tmux not found")
    d = tempfile.mkdtemp(prefix="rless-sqi-")
    make_fixtures(d)
    cwd = os.getcwd()
    os.chdir(d)
    passed = 0
    try:
        for task in TASKS:
            tid, desc, args, keys, ok = task[:5]
            w, h = task[5] if len(task) > 5 else (80, 24)
            screen = run_task(binary, args, keys, w, h)
            good = bool(ok(screen))
            passed += good
            print(f"{'PASS' if good else 'FAIL'} {tid} {desc}")
            if not good:
                for row in screen:
                    if row.strip():
                        print(f"       | {row}")
    finally:
        os.chdir(cwd)
        shutil.rmtree(d, ignore_errors=True)
    print(f"tasks: {len(TASKS)}  passed: {passed}  "
          f"rate: {passed / len(TASKS) * 100:.1f}%")


if __name__ == "__main__":
    main()
