#!/usr/bin/env python3
"""Mock sparks binary for CI harness smoke tests.

Implements a minimal subset of:
  sparks --config <path> dispatch --goal <...> --lane <...> --risk <...> --repo <...>

It writes a succeeded row into autonomous_task_outcomes and emits task_id=<uuid> on stderr.
"""

from __future__ import annotations

import argparse
import pathlib
import sqlite3
import sys
import uuid

try:
    import tomllib  # py3.11+
except ModuleNotFoundError:  # pragma: no cover
    tomllib = None


def parse_db_path(config_path: pathlib.Path) -> pathlib.Path:
    default = pathlib.Path("~/.sparks/sparks.db").expanduser()
    if not config_path.exists():
        return default
    text = config_path.read_text()
    if tomllib is not None:
        try:
            data = tomllib.loads(text)
            raw = data.get("db", {}).get("path")
            if raw:
                return pathlib.Path(raw).expanduser()
        except Exception:
            pass
    # Fallback parser for older Python without tomllib.
    in_db = False
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            in_db = line == "[db]"
            continue
        if in_db and line.startswith("path"):
            _, rhs = line.split("=", 1)
            value = rhs.strip().strip('"').strip("'")
            if value:
                return pathlib.Path(value).expanduser()
    return default


def ensure_schema(conn: sqlite3.Connection) -> None:
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS autonomous_task_outcomes (
            task_id TEXT PRIMARY KEY,
            lane TEXT NOT NULL,
            repo TEXT NOT NULL,
            risk_tier TEXT NOT NULL,
            ghost TEXT,
            goal TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'started',
            started_at TEXT NOT NULL DEFAULT (datetime('now')),
            finished_at TEXT,
            verification_total INTEGER NOT NULL DEFAULT 0,
            verification_passed INTEGER NOT NULL DEFAULT 0,
            rolled_back INTEGER NOT NULL DEFAULT 0,
            error TEXT
        )
        """
    )
    conn.commit()


def value_after(argv: list[str], flag: str, default: str) -> str:
    if flag not in argv:
        return default
    i = argv.index(flag)
    if i + 1 >= len(argv):
        return default
    return argv[i + 1]


def main() -> int:
    p = argparse.ArgumentParser(add_help=False)
    p.add_argument("--config", default="config.toml")
    ns, rest = p.parse_known_args()

    if "dispatch" not in rest:
        print("mock only supports dispatch", file=sys.stderr)
        return 2

    lane = value_after(rest, "--lane", "delivery")
    risk = value_after(rest, "--risk", "low")
    repo = value_after(rest, "--repo", "sparks")
    ghost = value_after(rest, "--ghost", "coder")
    goal = value_after(rest, "--goal", "mock goal")

    task_id = str(uuid.uuid4())
    db_path = parse_db_path(pathlib.Path(ns.config))
    if not db_path.is_absolute():
        db_path = (pathlib.Path.cwd() / db_path).resolve()
    db_path.parent.mkdir(parents=True, exist_ok=True)

    conn = sqlite3.connect(str(db_path))
    ensure_schema(conn)
    conn.execute(
        """
        INSERT OR REPLACE INTO autonomous_task_outcomes
          (task_id, lane, repo, risk_tier, ghost, goal, status, started_at)
        VALUES
          (?1, ?2, ?3, ?4, ?5, ?6, 'started', datetime('now'))
        """,
        (task_id, lane, repo, risk, ghost, goal),
    )
    conn.execute(
        """
        UPDATE autonomous_task_outcomes
           SET status='succeeded',
               finished_at=datetime('now'),
               verification_total=1,
               verification_passed=1,
               rolled_back=0,
               error=NULL
         WHERE task_id=?1
        """,
        (task_id,),
    )
    conn.commit()
    conn.close()

    print(
        "PLAN:\n- simulate dispatch execution\nEXECUTION:\n- wrote deterministic outcome row\n- no repo files changed"
    )
    print(f"Dispatched autonomous task to {ghost} (task_id={task_id}).", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
