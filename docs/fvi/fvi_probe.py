#!/usr/bin/env python3
"""FVI task probe for rless (docs/fvi/FVI_REPORT.md §2, tasks T01-T22).

Drives a binary in a fixed-size tmux pane (LANG=C.UTF-8), replays a fixed
key sequence per task and checks one success condition on the captured
screen (or, for T17/T21, on the process output / a written file). The same
script runs against the baseline and the new binary, so both columns of the
report come from one instrument.

This is a scripted functional check run by an AI agent, NOT a user test:
it shows whether a task *can* be completed with the documented keys.

usage: python3 docs/fvi/fvi_probe.py path/to/rless   (needs tmux; unsandboxed)
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

SOCK = "rless-fvi-probe"


def tmux(*args, check=True):
    return subprocess.run(["tmux", "-L", SOCK, *args], check=check,
                          capture_output=True, text=True).stdout


def run(binary, args, keys, width=80, height=24, cwd=None, pre=None,
        during=None, env=""):
    """Launches `binary args` in a pane, sends `keys` (tmux key names, or
    ("lit", text) for literal text), optionally runs `during()` part-way
    (after the keys) and returns the final screen lines."""
    tmux("kill-server", check=False)
    cmd = (f"cd {cwd}; env -u LESS -u LESSOPEN -u LESSCLOSE LESSHISTFILE=- "
           f"LANG=C.UTF-8 {env} {pre or ''}{binary} {args}; sleep 5")
    tmux("new-session", "-d", "-x", str(width), "-y", str(height), cmd)
    time.sleep(0.5)
    for k in keys:
        if isinstance(k, tuple):
            tmux("send-keys", "-l", "--", k[1])
        else:
            tmux("send-keys", k)
        time.sleep(0.2)
    if during:
        during()
        time.sleep(0.8)
    screen = tmux("capture-pane", "-p").split("\n")
    tmux("kill-server", check=False)
    return screen[:height]


def lit(s):
    return ("lit", s)


def make_fixtures(d):
    levels = ["INFO", "DEBUG", "ERROR", "WARN"]
    services = ["auth", "cart", "payment", "gateway"]
    with open(os.path.join(d, "logs.jsonl"), "w") as f:
        for i in range(1, 61):
            f.write(json.dumps({
                "level": levels[i % 4], "status": [200, 200, 500, 404][i % 4],
                "service": services[i % 4], "ms": (i * 37) % 900,
                "user": {"id": i % 7}, "msg": "db timeout" if i % 5 == 0 else "ok",
            }, separators=(",", ":")) + "\n")
    with open(os.path.join(d, "app.log"), "w") as f:
        for i in range(1, 201):
            f.write(f"{levels[i % 4]} request {i} handled\n")
    with open(os.path.join(d, "wide.txt"), "w") as f:
        for i in range(1, 30):
            f.write(f"row{i:02d} " + "x" * 100 + f" TAIL{i:02d}\n")
    with open(os.path.join(d, "short.txt"), "w") as f:
        f.write("only\nthree\nlines\n")
    shutil.copy(os.path.join(d, "app.log"), os.path.join(d, "grow.log"))


def has(screen, text):
    return any(text in line for line in screen)


def tasks(d):
    """(id, description, thunk returning bool)."""
    B = BIN

    def t01():
        s = run(B, "app.log", ["Space"], cwd=d)
        return has(s, "request 24 handled") and not has(s, "request 1 handled")

    def t02():
        s = run(B, "app.log", [lit("/request 150 "), "Enter"], cwd=d)
        return s[0].startswith("ERROR request 150 ")

    def t03():
        s = run(B, "-i app.log", [lit("/error request 2 "), "Enter"], cwd=d)
        return s[0].startswith("ERROR request 2 ")

    def t04():
        s = run(B, "app.log", [lit("&ERROR"), "Enter", "Enter"], cwd=d)
        return all(line.startswith("ERROR") for line in s[:23]) and has(s, "request 90 ")

    def t05():
        s = run(B, "app.log", [lit("&!INFO|DEBUG"), "Enter", "Enter"], cwd=d)
        body = s[:23]
        return all(l.startswith(("ERROR", "WARN")) for l in body) and has(body, "request 3 ")

    def t06():
        s = run(B, "app.log", [lit("&ERROR"), "Enter", "Enter", "j", "G"],
                height=6, cwd=d)
        return s[0].startswith("ERROR") and has(s, "request 198 ") and \
            all(l.startswith("ERROR") for l in s[:5])

    def t07():
        s = run(B, "logs.jsonl", [lit("&.status >= 500"), "Enter", "Enter"], cwd=d)
        return all('"status":500' in l for l in s[:15]) and has(s, '"status":500')

    def t08():
        s = run(B, "logs.jsonl",
                [lit('&.level == "ERROR" && .ms > 400 || .service == "auth"'),
                 "Enter", "Enter"], cwd=d)
        body = [l for l in s[:23] if l.startswith("{")]
        return bool(body) and all(
            '"service":"auth"' in l or '"level":"ERROR"' in l for l in body) and \
            not any('"service":"payment","ms":74,' in l for l in body)

    def t09():
        s = run(B, "logs.jsonl", [lit("&.user.id == 3"), "Enter", "Enter"], cwd=d)
        body = [l for l in s[:23] if l.startswith("{")]
        return bool(body) and all('"user":{"id":3}' in l for l in body)

    def t10():
        s = run(B, "logs.jsonl", [lit('&.msg =~ "time(out)?$"'), "Enter", "Enter"], cwd=d)
        # Rows are clipped to the pane, so count them and read the tag.
        body = [l for l in s[:23] if l.startswith("{")]
        return len(body) == 12 and has(s, "12 lines")

    def t11():
        grow = os.path.join(d, "grow.log")

        def append():
            with open(grow, "a") as f:
                f.write("APPENDED-LINE-1\nAPPENDED-LINE-2\n")
        s = run(B, "grow.log", ["F"], cwd=d, during=append)
        return has(s, "APPENDED-LINE-2")

    def t12():
        tmux("kill-server", check=False)
        cmd = (f"cd {d}; (seq 1 40; sleep 1; echo STREAMED-LATE; sleep 20) | "
               f"env -u LESS LANG=C.UTF-8 {B}; sleep 5")
        tmux("new-session", "-d", "-x", "80", "-y", "24", cmd)
        time.sleep(0.5)
        tmux("send-keys", "F")
        time.sleep(1.8)
        s = tmux("capture-pane", "-p").split("\n")
        tmux("kill-server", check=False)
        return has(s, "STREAMED-LATE")

    def t13():
        s = run(B, "logs.jsonl", ["c", "Enter", "x", "l", "l", "l", "s", "s", "q"],
                width=120, cwd=d)
        ms = []
        for line in s[:23]:
            cells = line.split("│")
            if len(cells) > 4 and cells[3].strip().isdigit():
                ms.append(int(cells[3].strip()))
        return len(ms) >= 10 and ms == sorted(ms, reverse=True)

    def t14():
        s = run(B, "logs.jsonl", ["x", "l", "i"], width=100, cwd=d)
        return has(s, "distinct 3") and has(s, "max 500")

    def t15():
        s = run(B, "-S wide.txt", ["Right"] * 5, width=40, height=6, cwd=d)
        return has(s, "TAIL01")

    def t16():
        a = run(B, "+G app.log", [], cwd=d)
        b = run(B, "-p 'request 120 ' app.log", [], cwd=d)
        return has(a, "request 200 ") and b[0].startswith("INFO request 120 ")

    def t17():
        out = subprocess.run(f"cd {d}; {B} short.txt | cat", shell=True,
                             capture_output=True, text=True).stdout
        s = run(B, "short.txt; echo EXITED=$?", [], cwd=d, env="LESS=-FX")
        return out == "only\nthree\nlines\n" and has(s, "EXITED=0") and has(s, "three")

    def t18():
        s = run(B, "app.log", [lit("-N"), "Enter"], height=5, cwd=d)
        return s[0].startswith("      1 ")

    def t19():
        s = run(B, "app.log", [lit("50g"), lit("ma"), "g", lit("'a")], height=5, cwd=d)
        return "request 50 " in s[0]

    def t20():
        s = run(B, "app.log", ["h"], cwd=d)
        return has(s, "follow") and has(s, "this help")

    def t21():
        out = os.path.join(d, "saved.log")
        if os.path.exists(out):
            os.remove(out)
        run(B, "app.log", [lit("&ERROR"), "Enter", "Enter", lit(":w saved.log"), "Enter"],
            cwd=d)
        if not os.path.exists(out):
            return False
        lines = open(out).read().splitlines()
        return len(lines) == 50 and all(l.startswith("ERROR") for l in lines)

    def t22():
        s = run(B, "logs.jsonl", ["c", "Enter", "x", "l", "p", "q", "H"], width=120, cwd=d)
        return s[0].startswith("status") and "│" in s[1]

    return [
        ("T01", "view and page", t01),
        ("T02", "regex search", t02),
        ("T03", "case-insensitive search -i", t03),
        ("T04", "&regex on plain text", t04),
        ("T05", "&!regex hides lines", t05),
        ("T06", "navigate inside a filter", t06),
        ("T07", "structured single predicate", t07),
        ("T08", "compound predicates", t08),
        ("T09", "nested JSON field", t09),
        ("T10", "regex on a field value", t10),
        ("T11", "follow a growing file", t11),
        ("T12", "follow a piped stream", t12),
        ("T13", "sort the table by a column", t13),
        ("T14", "column statistics", t14),
        ("T15", "horizontal scroll with -S", t15),
        ("T16", "+G / -p startup position", t16),
        ("T17", "git pager: non-tty copy, LESS=-FX", t17),
        ("T18", "toggle -N in the pager", t18),
        ("T19", "marks", t19),
        ("T20", "in-pager help", t20),
        ("T21", "save the filtered view", t21),
        ("T22", "table, pin, sticky header", t22),
    ]


if __name__ == "__main__":
    BIN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/release/rless")
    work = tempfile.mkdtemp(prefix="rless-fvi-")
    make_fixtures(work)
    passed = 0
    results = []
    for tid, desc, fn in tasks(work):
        try:
            ok = bool(fn())
        except Exception as e:  # a crash is a failed task, not a probe error
            ok = False
            desc += f" (error: {e})"
        passed += ok
        results.append((tid, ok))
        print(f"{tid} {'PASS' if ok else 'FAIL'}  {desc}")
    print(f"tasks passed: {passed}/{len(results)}")
    shutil.rmtree(work, ignore_errors=True)
