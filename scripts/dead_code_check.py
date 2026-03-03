#!/usr/bin/env python3
"""
Dead code gate for Athena.

Runs `cargo check` (and optionally `cargo check --features telegram`) and
fails if ANY dead_code warnings are emitted.  Call from CI after the normal
`cargo check` step so incremental caches are warm.

Usage:
    ./scripts/dead_code_check.py [--telegram]

Exit codes:
    0  – no dead code
    1  – dead code found or build failed
"""

import subprocess
import sys
import re

DEAD_CODE_PATTERN = re.compile(
    r"warning: (field|method|function|variant|methods|variants|fields|struct|enum|const|static|type alias|associated function) `[^`]+`.*? (is|are) never (read|used|constructed)"
)


def run_check(extra_args: list[str]) -> tuple[bool, list[str]]:
    """Run cargo check and return (success, list_of_dead_code_warnings)."""
    cmd = ["cargo", "check", "--message-format=human"] + extra_args
    result = subprocess.run(cmd, capture_output=True, text=True)
    output = result.stderr  # rustc emits warnings to stderr

    dead_warnings: list[str] = []
    for line in output.splitlines():
        if DEAD_CODE_PATTERN.search(line):
            dead_warnings.append(line.strip())

    build_ok = result.returncode == 0
    return build_ok, dead_warnings


def main() -> int:
    use_telegram = "--telegram" in sys.argv

    checks = [
        ("default", []),
    ]
    if use_telegram:
        checks.append(("telegram feature", ["--features", "telegram"]))

    all_clean = True

    for label, args in checks:
        print(f"\n── Dead code check ({label}) ──")
        build_ok, dead = run_check(args)

        if not build_ok:
            print(f"  FAIL: cargo check ({label}) did not compile")
            all_clean = False
            continue

        if dead:
            print(f"  FAIL: {len(dead)} dead code warning(s) found:")
            for w in dead:
                print(f"    {w}")
            all_clean = False
        else:
            print(f"  OK: no dead code")

    if all_clean:
        print("\nDead code gate: PASSED")
        return 0
    else:
        print("\nDead code gate: FAILED — remove or justify all dead code before merging")
        return 1


if __name__ == "__main__":
    sys.exit(main())
