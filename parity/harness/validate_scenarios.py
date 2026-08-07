#!/usr/bin/env python3
"""Static schema + coverage validator for parity/scenarios/*.json.

Checks, per scenario file:
  - required keys present (id, feature_ids, fixture, args, stdin, steps,
    quit_keys, expect_exit)
  - id uniqueness across the whole set
  - feature_ids are non-empty and every id exists in declared-inventory.json
  - fixture (and extra_fixtures) exist on disk, unless the scenario sets
    expect_fixture_missing: true (deliberate error-path scenario)
  - steps is non-empty; each step has at least one of keys/key/resize/capture
  - capture names are unique within a scenario
  - resize steps carry {"cols": int, "rows": int}
  - expect_exit is an int or null

Then, across the whole set:
  - every feature_id in docs/declared-inventory.json is covered by >=1 scenario
    (CONTRACT.md's Feature rule)

Exit 0 and print OK iff everything passes; otherwise print each violation and
exit 1.
"""
import glob
import json
import os
import sys

REPO_ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
SCENARIOS_DIR = os.path.join(REPO_ROOT, "parity", "scenarios")
INVENTORY_PATH = os.path.join(REPO_ROOT, "docs", "declared-inventory.json")

REQUIRED_KEYS = ["id", "feature_ids", "fixture", "args", "stdin", "steps", "quit_keys", "expect_exit"]
VALID_NAMED_KEYS = {"Enter", "Space", "Up", "Down", "PPage", "NPage"}


def main() -> None:
    errors = []
    warnings = []

    with open(INVENTORY_PATH, encoding="utf-8") as f:
        inventory = json.load(f)
    inventory_ids = {feat["feature_id"] for feat in inventory["features"]}

    files = sorted(glob.glob(os.path.join(SCENARIOS_DIR, "*.json")))
    if not files:
        print(f"error: no scenario files found in {SCENARIOS_DIR}", file=sys.stderr)
        sys.exit(2)

    seen_ids = {}
    seen_ids_lower = {}
    covered_features = set()
    unknown_feature_uses = []

    for path in files:
        rel = os.path.relpath(path, REPO_ROOT)
        try:
            with open(path, encoding="utf-8") as f:
                data = json.load(f)
        except json.JSONDecodeError as e:
            errors.append(f"{rel}: invalid JSON: {e}")
            continue

        for key in REQUIRED_KEYS:
            if key not in data:
                errors.append(f"{rel}: missing required key '{key}'")

        sid = data.get("id")
        if sid:
            if sid in seen_ids:
                errors.append(f"{rel}: duplicate scenario id '{sid}' (also in {seen_ids[sid]})")
            else:
                seen_ids[sid] = rel
            sid_lower = sid.lower()
            if sid_lower in seen_ids_lower and seen_ids_lower[sid_lower][0] != sid:
                other_id, other_rel = seen_ids_lower[sid_lower]
                errors.append(
                    f"{rel}: scenario id '{sid}' differs only by case from '{other_id}' ({other_rel}) - "
                    "collides as a filename on case-insensitive filesystems (e.g. default macOS APFS)"
                )
            else:
                seen_ids_lower[sid_lower] = (sid, rel)

        fids = data.get("feature_ids")
        if fids is not None:
            if not isinstance(fids, list) or not fids:
                errors.append(f"{rel}: 'feature_ids' must be a non-empty array")
            else:
                for fid in fids:
                    if fid in inventory_ids:
                        covered_features.add(fid)
                    else:
                        unknown_feature_uses.append((rel, fid))

        fixture = data.get("fixture")
        fixture_paths = ([fixture] if fixture else []) + data.get("extra_fixtures", [])
        if not data.get("expect_fixture_missing", False):
            for fp in fixture_paths:
                if not os.path.isfile(os.path.join(REPO_ROOT, fp)):
                    errors.append(f"{rel}: fixture not found on disk: {fp}")

        if data.get("stdin") and not fixture:
            errors.append(f"{rel}: stdin=true requires 'fixture'")

        steps = data.get("steps")
        if steps is not None:
            if not isinstance(steps, list) or not steps:
                errors.append(f"{rel}: 'steps' must be a non-empty array")
            else:
                capture_names = []
                for i, step in enumerate(steps):
                    if not isinstance(step, dict):
                        errors.append(f"{rel}: step[{i}] is not an object")
                        continue
                    if not any(k in step for k in ("keys", "key", "resize", "capture")):
                        errors.append(f"{rel}: step[{i}] has none of keys/key/resize/capture (no-op step)")
                    if "key" in step and step["key"] not in VALID_NAMED_KEYS:
                        warnings.append(
                            f"{rel}: step[{i}] uses non-standard named key '{step['key']}' "
                            f"(CONTRACT.md lists {sorted(VALID_NAMED_KEYS)})"
                        )
                    if "key_after" in step and step["key_after"] not in VALID_NAMED_KEYS:
                        warnings.append(f"{rel}: step[{i}] key_after '{step['key_after']}' not in standard named key set")
                    if "capture" in step:
                        name = step["capture"]
                        if name in capture_names:
                            errors.append(f"{rel}: duplicate capture name '{name}' within scenario")
                        capture_names.append(name)
                    if "resize" in step:
                        r = step["resize"]
                        if not (isinstance(r, dict) and isinstance(r.get("cols"), int) and isinstance(r.get("rows"), int)):
                            errors.append(f"{rel}: step[{i}] 'resize' must be {{'cols': int, 'rows': int}}")

        expect_exit = data.get("expect_exit")
        if "expect_exit" in data and expect_exit is not None and not isinstance(expect_exit, int):
            errors.append(f"{rel}: 'expect_exit' must be an integer or null")

    for rel, fid in unknown_feature_uses:
        errors.append(f"{rel}: feature_id '{fid}' not present in declared-inventory.json")

    missing_coverage = inventory_ids - covered_features
    print(f"scenario files: {len(files)}")
    print(f"feature coverage: {len(covered_features)}/{len(inventory_ids)}")
    if missing_coverage:
        errors.append(f"inventory features with zero scenario coverage: {sorted(missing_coverage)}")

    for w in warnings:
        print(f"WARN {w}")

    if errors:
        print(f"FAILED with {len(errors)} error(s):")
        for e in errors:
            print(f"  - {e}")
        sys.exit(1)

    print("OK: all scenario files valid, full feature coverage confirmed")
    sys.exit(0)


if __name__ == "__main__":
    main()
