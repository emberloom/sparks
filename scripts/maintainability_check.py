#!/usr/bin/env python3
import argparse
import json
import re
import sys
from pathlib import Path


FN_RE = re.compile(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+([A-Za-z0-9_]+)\b")


def rust_files(root: Path):
    src = root / "src"
    return sorted(src.rglob("*.rs"))


def function_spans(lines):
    funcs = []
    i = 0
    while i < len(lines):
        m = FN_RE.match(lines[i])
        if not m:
            i += 1
            continue
        name = m.group(1)
        start = i + 1
        brace = 0
        seen_open = False
        j = i
        while j < len(lines):
            line = lines[j]
            opens = line.count("{")
            closes = line.count("}")
            if opens > 0:
                seen_open = True
            brace += opens - closes
            if seen_open and brace <= 0:
                end = j + 1
                funcs.append(
                    {
                        "name": name,
                        "start": start,
                        "end": end,
                        "length": end - start + 1,
                    }
                )
                i = j + 1
                break
            j += 1
        else:
            end = len(lines)
            funcs.append(
                {
                    "name": name,
                    "start": start,
                    "end": end,
                    "length": end - start + 1,
                }
            )
            i = len(lines)
    return funcs


def compute_metrics(root: Path):
    files = rust_files(root)
    by_file = {}
    all_funcs = []
    for f in files:
        rel = str(f.relative_to(root))
        lines = f.read_text(encoding="utf-8", errors="ignore").splitlines()
        funcs = function_spans(lines)
        by_file[rel] = {
            "loc": len(lines),
            "fn_count": len(funcs),
            "max_fn_len": max((fn["length"] for fn in funcs), default=0),
            "fn_over_80": sum(1 for fn in funcs if fn["length"] > 80),
            "fn_over_120": sum(1 for fn in funcs if fn["length"] > 120),
        }
        for fn in funcs:
            fn_copy = dict(fn)
            fn_copy["file"] = rel
            all_funcs.append(fn_copy)

    total_loc = sum(v["loc"] for v in by_file.values())
    max_file = max(by_file.items(), key=lambda kv: kv[1]["loc"], default=("", {"loc": 0}))
    max_fn = max(all_funcs, key=lambda fn: fn["length"], default=None)

    summary = {
        "files": len(by_file),
        "total_loc": total_loc,
        "functions": len(all_funcs),
        "fn_over_80": sum(1 for fn in all_funcs if fn["length"] > 80),
        "fn_over_120": sum(1 for fn in all_funcs if fn["length"] > 120),
        "files_over_1000_loc": sum(1 for v in by_file.values() if v["loc"] > 1000),
        "max_file_loc": max_file[1]["loc"],
        "max_file_path": max_file[0],
        "max_fn_len": (max_fn or {}).get("length", 0),
        "max_fn_name": (max_fn or {}).get("name", ""),
        "max_fn_file": (max_fn or {}).get("file", ""),
    }

    return {
        "summary": summary,
        "by_file": by_file,
    }


def print_report(metrics):
    s = metrics["summary"]
    print("Maintainability Summary")
    print(f"files={s['files']} total_loc={s['total_loc']} functions={s['functions']}")
    print(f"fn_over_80={s['fn_over_80']} fn_over_120={s['fn_over_120']}")
    print(f"files_over_1000_loc={s['files_over_1000_loc']}")
    print(f"max_file={s['max_file_path']} ({s['max_file_loc']})")
    print(f"max_function={s['max_fn_file']}::{s['max_fn_name']} ({s['max_fn_len']})")

    top_files = sorted(
        metrics["by_file"].items(),
        key=lambda kv: (kv[1]["loc"], kv[1]["max_fn_len"]),
        reverse=True,
    )[:10]
    print("\nTop files by LOC")
    for path, info in top_files:
        print(
            f"{info['loc']:4d} {path} "
            f"max_fn={info['max_fn_len']:3d} >80={info['fn_over_80']:2d} >120={info['fn_over_120']:2d}"
        )


def regression_failures(current, baseline):
    cs = current["summary"]
    bs = baseline["summary"]
    checks = [
        "fn_over_80",
        "fn_over_120",
        "max_fn_len",
    ]
    failures = []
    for key in checks:
        if cs.get(key, 0) > bs.get(key, 0):
            failures.append((key, bs.get(key, 0), cs.get(key, 0)))
    return failures


def main():
    parser = argparse.ArgumentParser(description="Maintainability metrics + regression gate")
    parser.add_argument("--root", default=".", help="repository root")
    parser.add_argument(
        "--baseline",
        default="docs/maintainability-baseline.json",
        help="baseline metrics file",
    )
    parser.add_argument(
        "--write-baseline",
        action="store_true",
        help="write current metrics as baseline and exit",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="print full metrics JSON",
    )
    parser.add_argument(
        "--no-fail",
        action="store_true",
        help="never fail on regressions",
    )
    args = parser.parse_args()

    root = Path(args.root).resolve()
    metrics = compute_metrics(root)

    if args.write_baseline:
        path = root / args.baseline
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(metrics, indent=2) + "\n", encoding="utf-8")
        print(f"Wrote baseline: {path}")
        return 0

    if args.json:
        print(json.dumps(metrics, indent=2))
    else:
        print_report(metrics)

    baseline_path = root / args.baseline
    if not baseline_path.exists():
        print(f"\nNo baseline found at {baseline_path}; skipping regression gate.")
        return 0

    baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
    failures = regression_failures(metrics, baseline)
    if failures and not args.no_fail:
        print("\nRegression(s) detected:")
        for key, old, new in failures:
            print(f"- {key}: baseline={old} current={new}")
        return 1

    if failures:
        print("\nRegressions detected but ignored due to --no-fail.")
    else:
        print("\nNo maintainability regressions vs baseline.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
