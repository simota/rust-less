#!/usr/bin/env python3
"""Compare two run.sh output directories (baseline vs candidate) per
scenario/capture and emit a PASS/FAIL matrix.

Usage: diff.py <baseline-dir> <candidate-dir>

Reads meta.json (written by run.py) from each baseline scenario directory
for feature_ids / compare_attrs. Writes <candidate-dir>/_diff-report/matrix.json
(feature_id -> scenario_id -> "PASS"/"FAIL") and per-mismatch unified diffs
under <candidate-dir>/_diff-report/<scenario_id>/. Exit code 0 iff every
scenario passed.
"""
import json
import os
import subprocess
import sys


def read_exit_code(path: str):
    if not os.path.exists(path):
        return None
    with open(path, encoding="utf-8") as f:
        return f.read().strip()


def diff_files(a: str, b: str):
    """Returns (passed, diff_text_or_reason)."""
    a_exists, b_exists = os.path.exists(a), os.path.exists(b)
    if not a_exists or not b_exists:
        missing = "baseline" if not a_exists else "candidate"
        return False, f"MISSING: {missing} file absent ({a if not a_exists else b})"
    with open(a, "rb") as fa, open(b, "rb") as fb:
        if fa.read() == fb.read():
            return True, None
    result = subprocess.run(["diff", "-u", a, b], capture_output=True, text=True)
    return False, result.stdout


def main() -> None:
    if len(sys.argv) != 3:
        print("usage: diff.py <baseline-dir> <candidate-dir>", file=sys.stderr)
        sys.exit(2)
    baseline_dir, candidate_dir = sys.argv[1], sys.argv[2]

    def scenario_ids(d):
        return sorted(
            e for e in os.listdir(d)
            if os.path.isdir(os.path.join(d, e)) and not e.startswith("_")
        )

    baseline_ids = scenario_ids(baseline_dir)
    candidate_ids = set(scenario_ids(candidate_dir))

    report_dir = os.path.join(candidate_dir, "_diff-report")
    os.makedirs(report_dir, exist_ok=True)

    matrix = {}
    total = 0
    passed = 0
    failures = []

    for sid in baseline_ids:
        total += 1
        b_sdir = os.path.join(baseline_dir, sid)
        c_sdir = os.path.join(candidate_dir, sid)
        scenario_pass = True
        reasons = []
        feature_ids = []
        compare_attrs = False

        if sid not in candidate_ids:
            scenario_pass = False
            reasons.append("candidate scenario dir missing")
            meta_path = os.path.join(b_sdir, "meta.json")
            if os.path.exists(meta_path):
                with open(meta_path, encoding="utf-8") as f:
                    feature_ids = json.load(f).get("feature_ids", [])
        else:
            meta_path = os.path.join(b_sdir, "meta.json")
            meta = {}
            if os.path.exists(meta_path):
                with open(meta_path, encoding="utf-8") as f:
                    meta = json.load(f)
            feature_ids = meta.get("feature_ids", [])
            compare_attrs = meta.get("compare_attrs", False)

            capture_names = sorted(
                fn[:-4] for fn in os.listdir(b_sdir)
                if fn.endswith(".txt") and not fn.endswith(".attrs.txt")
            )
            for name in capture_names:
                ok, detail = diff_files(
                    os.path.join(b_sdir, f"{name}.txt"), os.path.join(c_sdir, f"{name}.txt")
                )
                if not ok:
                    scenario_pass = False
                    reasons.append(f"capture '{name}' .txt mismatch")
                    out_path = os.path.join(report_dir, sid, f"{name}.txt.diff")
                    os.makedirs(os.path.dirname(out_path), exist_ok=True)
                    with open(out_path, "w", encoding="utf-8") as f:
                        f.write(detail or "")

                if compare_attrs:
                    ok2, detail2 = diff_files(
                        os.path.join(b_sdir, f"{name}.attrs.txt"),
                        os.path.join(c_sdir, f"{name}.attrs.txt"),
                    )
                    if not ok2:
                        scenario_pass = False
                        reasons.append(f"capture '{name}' .attrs.txt mismatch")
                        out_path = os.path.join(report_dir, sid, f"{name}.attrs.txt.diff")
                        os.makedirs(os.path.dirname(out_path), exist_ok=True)
                        with open(out_path, "w", encoding="utf-8") as f:
                            f.write(detail2 or "")

            b_exit = read_exit_code(os.path.join(b_sdir, "exit_code"))
            c_exit = read_exit_code(os.path.join(c_sdir, "exit_code"))
            if b_exit != c_exit:
                scenario_pass = False
                reasons.append(f"exit_code mismatch: baseline={b_exit!r} candidate={c_exit!r}")

        if scenario_pass:
            passed += 1
        else:
            failures.append((sid, reasons))

        for fid in feature_ids:
            matrix.setdefault(fid, {})[sid] = "PASS" if scenario_pass else "FAIL"

    extra_candidate_only = sorted(candidate_ids - set(baseline_ids))

    matrix_path = os.path.join(report_dir, "matrix.json")
    with open(matrix_path, "w", encoding="utf-8") as f:
        json.dump(matrix, f, indent=2, sort_keys=True)
        f.write("\n")

    print(f"scenarios: {total}  passed: {passed}  failed: {len(failures)}")
    for sid, reasons in failures:
        print(f"FAIL {sid}: {'; '.join(reasons)}")
    if extra_candidate_only:
        print(f"NOTE candidate-only scenario dirs (not in baseline, not scored): {extra_candidate_only}")
    print(f"matrix written to {matrix_path}")

    sys.exit(0 if not failures else 1)


if __name__ == "__main__":
    main()
