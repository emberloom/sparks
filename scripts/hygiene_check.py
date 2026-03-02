#!/usr/bin/env python3
"""
Hygiene check for Athena — catches common AI-generated code smells.

Zero-tolerance checks (must stay at 0):
  - todo!() / unimplemented!() outside test blocks
  - dbg!() anywhere
  - #![allow(...)] crate-level suppressors
  - unsafe blocks without a preceding // SAFETY: comment
  - use ...::* glob imports outside test blocks

Trend-tracked checks (fail if count grows beyond baseline):
  - .unwrap() calls outside test blocks
  - .expect(...) calls outside test blocks
  - #[allow(clippy::too_many_arguments)] annotations
  - #[allow(dead_code)] item-level annotations

Run with --update-baseline to record the current counts as the new baseline.
"""

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).parent.parent
SRC = ROOT / "src"
BASELINE_PATH = ROOT / "docs" / "hygiene-baseline.json"

RESET = "\033[0m"
RED = "\033[1;31m"
GREEN = "\033[1;32m"
YELLOW = "\033[1;33m"
CYAN = "\033[1;36m"
DIM = "\033[2m"


# ---------------------------------------------------------------------------
# Test-block stripping
# ---------------------------------------------------------------------------

def non_test_lines(path: Path) -> list[tuple[int, str]]:
    """
    Return (1-indexed lineno, text) pairs for lines that are NOT inside a
    #[cfg(test)] block, mod tests block, or #[test] function.
    """
    lines = path.read_text(errors="replace").splitlines()
    result: list[tuple[int, str]] = []

    i = 0
    pending_cfg_test = False
    pending_test_fn = False

    while i < len(lines):
        line = lines[i]
        stripped = line.strip()

        # ── Detect #[cfg(test)] ──────────────────────────────────────────
        if stripped == "#[cfg(test)]":
            pending_cfg_test = True
            i += 1
            continue

        # ── Skip the mod block that follows #[cfg(test)] ─────────────────
        if pending_cfg_test and re.match(r"(?:pub\s+)?mod\s+\w+", stripped):
            depth = 0
            while i < len(lines):
                depth += lines[i].count("{") - lines[i].count("}")
                i += 1
                if depth <= 0 and i > 0:
                    break
            pending_cfg_test = False
            continue

        # ── Skip bare `mod tests { ... }` (without cfg(test)) ────────────
        if re.match(r"(?:pub\s+)?mod\s+tests\s*\{", stripped):
            depth = 0
            while i < len(lines):
                depth += lines[i].count("{") - lines[i].count("}")
                i += 1
                if depth <= 0:
                    break
            pending_cfg_test = False
            continue

        # ── Detect #[test] attribute ──────────────────────────────────────
        if re.match(r"#\[(?:tokio::)?test\b", stripped):
            pending_test_fn = True
            i += 1
            continue

        # ── Skip the fn body that follows #[test] ─────────────────────────
        if pending_test_fn and re.match(r"(?:pub\s+)?(?:async\s+)?fn\s+\w+", stripped):
            depth = 0
            seen_open = False
            while i < len(lines):
                opens = lines[i].count("{")
                closes = lines[i].count("}")
                depth += opens - closes
                if opens:
                    seen_open = True
                i += 1
                if seen_open and depth <= 0:
                    break
            pending_test_fn = False
            continue

        pending_cfg_test = False
        pending_test_fn = False
        result.append((i + 1, line))
        i += 1

    return result


def all_lines(path: Path) -> list[tuple[int, str]]:
    """Return all (lineno, text) pairs including test blocks."""
    return [(i + 1, l) for i, l in enumerate(path.read_text(errors="replace").splitlines())]


def rust_files() -> list[Path]:
    return sorted(SRC.rglob("*.rs"))


# ---------------------------------------------------------------------------
# Individual checks
# ---------------------------------------------------------------------------

def check_todo_unimplemented() -> list[str]:
    """todo!() / unimplemented!() outside test blocks."""
    pattern = re.compile(r"\b(?:todo|unimplemented)!\s*\(")
    hits = []
    for path in rust_files():
        for lineno, line in non_test_lines(path):
            if pattern.search(line):
                hits.append(f"  {path.relative_to(ROOT)}:{lineno}: {line.strip()}")
    return hits


def check_dbg() -> list[str]:
    """dbg!() anywhere (even in tests — it should never be committed)."""
    pattern = re.compile(r"\bdbg!\s*\(")
    hits = []
    for path in rust_files():
        for lineno, line in all_lines(path):
            if pattern.search(line):
                hits.append(f"  {path.relative_to(ROOT)}:{lineno}: {line.strip()}")
    return hits


def check_crate_allow() -> list[str]:
    """#![allow(...)] crate-level suppressors."""
    pattern = re.compile(r"#!\[allow\(")
    hits = []
    for path in rust_files():
        for lineno, line in all_lines(path):
            if pattern.search(line):
                hits.append(f"  {path.relative_to(ROOT)}:{lineno}: {line.strip()}")
    return hits


def check_unsafe_without_safety() -> list[str]:
    """unsafe blocks / fns without a preceding // SAFETY: comment."""
    unsafe_re = re.compile(r"\bunsafe\s*(?:\{|fn\b|impl\b|trait\b)")
    hits = []
    for path in rust_files():
        lines = path.read_text(errors="replace").splitlines()
        for i, line in enumerate(lines):
            if unsafe_re.search(line):
                # Check the preceding non-blank line for // SAFETY:
                prev = ""
                j = i - 1
                while j >= 0 and not lines[j].strip():
                    j -= 1
                if j >= 0:
                    prev = lines[j].strip()
                if not prev.startswith("// SAFETY:"):
                    hits.append(
                        f"  {path.relative_to(ROOT)}:{i + 1}: {line.strip()}"
                    )
    return hits


def check_glob_imports() -> list[str]:
    """use ...::* glob imports outside test blocks (use super::* in tests is fine).

    Lines ending with '// hygiene: allow' are explicitly exempted (e.g. crate
    conventions like `use teloxide::prelude::*`).
    """
    pattern = re.compile(r"^\s*use\s+.+::\*\s*;")
    hits = []
    for path in rust_files():
        for lineno, line in non_test_lines(path):
            if "// hygiene: allow" in line:
                continue
            if pattern.match(line):
                hits.append(f"  {path.relative_to(ROOT)}:{lineno}: {line.strip()}")
    return hits


# ---------------------------------------------------------------------------
# Trend-tracked counts
# ---------------------------------------------------------------------------

def count_unwrap() -> int:
    pattern = re.compile(r"\.unwrap\(\)")
    count = 0
    for path in rust_files():
        for _, line in non_test_lines(path):
            count += len(pattern.findall(line))
    return count


def count_expect() -> int:
    pattern = re.compile(r"\.expect\s*\(")
    count = 0
    for path in rust_files():
        for _, line in non_test_lines(path):
            count += len(pattern.findall(line))
    return count


def count_too_many_args_allow() -> int:
    pattern = re.compile(r"#\[allow\(clippy::too_many_arguments\)\]")
    count = 0
    for path in rust_files():
        for _, line in all_lines(path):
            if pattern.search(line):
                count += 1
    return count


def count_dead_code_allow() -> int:
    """Item-level #[allow(dead_code)] — NOT #[cfg_attr(..., allow(dead_code))]."""
    pattern = re.compile(r"^\s*#\[allow\(dead_code\)\]")
    count = 0
    for path in rust_files():
        for _, line in all_lines(path):
            if pattern.match(line):
                count += 1
    return count


# ---------------------------------------------------------------------------
# Baseline I/O
# ---------------------------------------------------------------------------

def load_baseline() -> dict:
    if BASELINE_PATH.exists():
        return json.loads(BASELINE_PATH.read_text())
    return {}


def save_baseline(data: dict) -> None:
    BASELINE_PATH.write_text(json.dumps(data, indent=2) + "\n")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--update-baseline",
        action="store_true",
        help="Record current counts as the new baseline and exit 0.",
    )
    args = parser.parse_args()

    print(f"{CYAN}Athena hygiene check{RESET}")
    print(f"{DIM}src: {SRC}{RESET}\n")

    failures: list[str] = []
    passes: list[str] = []

    # ── Zero-tolerance ───────────────────────────────────────────────────
    def zero(label: str, hits: list[str]) -> None:
        if hits:
            failures.append(f"{label} ({len(hits)} hit(s)):")
            failures.extend(hits[:10])
            if len(hits) > 10:
                failures.append(f"  ... and {len(hits) - 10} more")
        else:
            passes.append(f"{label} — clean")

    zero("todo!/unimplemented! outside tests", check_todo_unimplemented())
    zero("dbg! anywhere", check_dbg())
    zero("#![allow(...)] crate-level", check_crate_allow())
    zero("unsafe without // SAFETY:", check_unsafe_without_safety())
    zero("glob imports outside tests", check_glob_imports())

    # ── Trend-tracked ────────────────────────────────────────────────────
    baseline = load_baseline()

    current = {
        "unwrap_non_test": count_unwrap(),
        "expect_non_test": count_expect(),
        "too_many_args_allow": count_too_many_args_allow(),
        "dead_code_allow": count_dead_code_allow(),
    }

    if args.update_baseline:
        save_baseline(current)
        print(f"{GREEN}Baseline updated:{RESET}")
        for k, v in current.items():
            print(f"  {k}: {v}")
        return 0

    labels = {
        "unwrap_non_test": ".unwrap() outside tests",
        "expect_non_test": ".expect() outside tests",
        "too_many_args_allow": "#[allow(clippy::too_many_arguments)]",
        "dead_code_allow": "#[allow(dead_code)] item-level",
    }

    for key, label in labels.items():
        cur = current[key]
        base = baseline.get(key)
        if base is None:
            passes.append(f"{label}: {cur} (no baseline — run --update-baseline)")
        elif cur > base:
            failures.append(
                f"{label}: {cur} (baseline {base}, regression +{cur - base})"
            )
        else:
            passes.append(f"{label}: {cur} (baseline {base})")

    # ── Report ───────────────────────────────────────────────────────────
    for msg in passes:
        print(f"  {GREEN}✓{RESET} {msg}")

    if failures:
        print()
        for msg in failures:
            if msg.startswith("  "):
                print(f"{RED}{msg}{RESET}")
            else:
                print(f"  {RED}✗{RESET} {msg}")
        print(f"\n{RED}Hygiene check FAILED{RESET}")
        return 1

    print(f"\n{GREEN}All hygiene checks passed{RESET}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
