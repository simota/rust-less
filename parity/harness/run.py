#!/usr/bin/env python3
"""Scenario runner: drives <binary> under tmux per parity/harness/CONTRACT.md
and parity/scenarios/*.json, writing per-scenario capture artifacts.

Usage: run.py <binary> <scenarios-dir|scenario.json> <outdir>

NOTE for the hub operator: this script issues real `tmux` commands against a
dedicated socket ($TMPDIR/rust-less-parity.sock) and must be run outside the
agent sandbox (Unix-socket restricted). See CONTRACT.md and
parity/harness/EXTENSIONS.md for the handful of additive fields/behaviors
this runner supports beyond the base scenario schema.
"""
import glob
import json
import os
import shlex
import subprocess
import sys
import time

DEFAULT_WAIT_MS = 400
DEFAULT_COLS = 80
DEFAULT_ROWS = 24
EXIT_POLL_TIMEOUT_S = 2.0
EXIT_POLL_INTERVAL_S = 0.05

# Per CONTRACT.md canonicalization contract.
ENV_PREFIX = ["env", "-u", "LESS", "-u", "LESSOPEN", "-u", "LESSCLOSE",
              "LESSHISTFILE=-", "LANG=C.UTF-8"]


def sock_path() -> str:
    tmpdir = os.environ.get("TMPDIR", "/tmp")
    return os.path.join(tmpdir, "rust-less-parity.sock")


def tmux(*args, check=True):
    cmd = ["tmux", "-S", sock_path()] + list(args)
    result = subprocess.run(cmd, capture_output=True, text=True)
    if check and result.returncode != 0:
        raise RuntimeError(
            f"tmux command failed: {' '.join(shlex.quote(a) for a in cmd)}\n"
            f"stderr: {result.stderr}"
        )
    return result


def load_scenarios(path):
    if os.path.isdir(path):
        files = sorted(glob.glob(os.path.join(path, "*.json")))
    else:
        files = [path]
    scenarios = []
    for f in files:
        with open(f, "r", encoding="utf-8") as fh:
            data = json.load(fh)
        data["_source_file"] = f
        scenarios.append(data)
    return scenarios


def build_inner_script(binary: str, scenario: dict, exit_code_path: str) -> str:
    """Build the shell pipeline that launches the binary and records its
    exit code to a real file (uniform for stdin and non-stdin scenarios;
    see EXTENSIONS.md for why this generalizes the stdin-only wording in
    CONTRACT.md)."""
    args = scenario.get("args", [])
    fixtures = []
    if scenario.get("fixture"):
        fixtures.append(scenario["fixture"])
    fixtures += scenario.get("extra_fixtures", [])
    stdin = scenario.get("stdin", False)
    env_str = " ".join(shlex.quote(x) for x in ENV_PREFIX)

    if stdin:
        if not fixtures:
            raise ValueError(f"scenario {scenario.get('id')}: stdin=true requires 'fixture'")
        primary = fixtures[0]
        binary_str = " ".join(shlex.quote(x) for x in [binary] + args)
        inner = f"cat {shlex.quote(primary)} | {env_str} {binary_str}"
    else:
        full = [binary] + args + fixtures
        inner = env_str + " " + " ".join(shlex.quote(x) for x in full)

    return f"{inner}; echo $? > {shlex.quote(exit_code_path)}"


def run_scenario(binary: str, scenario: dict, outdir: str) -> None:
    sid = scenario["id"]
    sdir = os.path.join(outdir, sid)
    os.makedirs(sdir, exist_ok=True)
    exit_code_path = os.path.join(sdir, "exit_code")
    log_lines = []

    def log(msg: str) -> None:
        log_lines.append(msg)
        print(f"  {msg}")

    inner_script = build_inner_script(binary, scenario, exit_code_path)
    pane_cmd = "sh -c " + shlex.quote(inner_script)

    cols, rows = scenario.get("initial_size", [DEFAULT_COLS, DEFAULT_ROWS])
    session = f"parity-{sid}-{os.getpid()}"
    default_wait_ms = scenario.get("wait_ms", DEFAULT_WAIT_MS)

    log(f"LAUNCH session={session} size={cols}x{rows} cmd={inner_script}")
    tmux("new-session", "-d", "-s", session, "-x", str(cols), "-y", str(rows), pane_cmd)

    try:
        time.sleep(default_wait_ms / 1000.0)  # settle wait after launch, same policy as steps

        for step in scenario.get("steps", []):
            if "resize" in step:
                r = step["resize"]
                log(f"RESIZE {r['cols']}x{r['rows']}")
                tmux("resize-window", "-t", session, "-x", str(r["cols"]), "-y", str(r["rows"]), check=False)
            if "keys" in step:
                log(f"SEND keys={step['keys']!r}")
                tmux("send-keys", "-t", session, "-l", "--", step["keys"], check=False)
            if "key" in step:
                log(f"SEND key={step['key']}")
                tmux("send-keys", "-t", session, step["key"], check=False)
            if "key_after" in step:
                log(f"SEND key_after={step['key_after']}")
                tmux("send-keys", "-t", session, step["key_after"], check=False)

            wait_ms = step.get("wait_ms", default_wait_ms)
            time.sleep(wait_ms / 1000.0)

            if "capture" in step:
                name = step["capture"]
                plain = tmux("capture-pane", "-t", session, "-p", check=False)
                attrs = tmux("capture-pane", "-t", session, "-p", "-e", check=False)
                with open(os.path.join(sdir, f"{name}.txt"), "w", encoding="utf-8") as f:
                    f.write(plain.stdout if plain.returncode == 0 else "")
                with open(os.path.join(sdir, f"{name}.attrs.txt"), "w", encoding="utf-8") as f:
                    f.write(attrs.stdout if attrs.returncode == 0 else "")
                if plain.returncode != 0:
                    log(f"WARN capture '{name}' failed (pane/session likely gone): {plain.stderr.strip()}")
                else:
                    log(f"CAPTURE {name}")

        quit_keys = scenario.get("quit_keys")
        if quit_keys:
            log(f"SEND quit_keys={quit_keys!r}")
            tmux("send-keys", "-t", session, "-l", "--", quit_keys, check=False)

        deadline = time.time() + EXIT_POLL_TIMEOUT_S
        found = False
        while time.time() < deadline:
            if os.path.exists(exit_code_path) and os.path.getsize(exit_code_path) > 0:
                found = True
                break
            time.sleep(EXIT_POLL_INTERVAL_S)
        if found:
            log(f"EXIT_CODE {open(exit_code_path).read().strip()}")
        else:
            log("WARN exit_code not observed within 2s poll window (TIMEOUT)")
    finally:
        tmux("kill-session", "-t", session, check=False)

    with open(os.path.join(sdir, "steps.log"), "w", encoding="utf-8") as f:
        f.write("\n".join(log_lines) + "\n")

    meta = {
        "feature_ids": scenario.get("feature_ids", []),
        "compare_attrs": scenario.get("compare_attrs", False),
        "expect_exit": scenario.get("expect_exit"),
    }
    with open(os.path.join(sdir, "meta.json"), "w", encoding="utf-8") as f:
        json.dump(meta, f, indent=2)
        f.write("\n")


def main() -> None:
    if len(sys.argv) != 4:
        print("usage: run.py <binary> <scenarios-dir|scenario.json> <outdir>", file=sys.stderr)
        sys.exit(2)
    binary, scenarios_path, outdir = sys.argv[1:4]
    os.makedirs(outdir, exist_ok=True)

    scenarios = load_scenarios(scenarios_path)
    if not scenarios:
        print(f"error: no scenarios found at {scenarios_path}", file=sys.stderr)
        sys.exit(2)

    failed = []
    for scenario in scenarios:
        sid = scenario.get("id", scenario.get("_source_file", "?"))
        print(f"[run] {sid}")
        try:
            run_scenario(binary, scenario, outdir)
        except Exception as exc:  # noqa: BLE001 - one bad scenario must not abort the batch
            print(f"[run] ERROR in scenario {sid}: {exc}", file=sys.stderr)
            failed.append(sid)

    print(f"[run] done: {len(scenarios)} scenario(s) -> {outdir}"
          + (f"  ({len(failed)} errored: {failed})" if failed else ""))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
