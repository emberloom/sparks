#!/usr/bin/env python3
"""Run a deterministic user-flow for ticket intake + writeback.

Flow:
- Start mock Linear GraphQL server
- Start Athena with webhook enabled + mock dispatch
- Send Linear webhook payload
- Wait for writeback (comment + status) to hit mock server ledger
"""

from __future__ import annotations

import argparse
import hmac
import json
import os
import socket
import sqlite3
import subprocess
import sys
import time
from hashlib import sha256
from pathlib import Path
from typing import Any, Dict, List
from urllib import request

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_ISSUE_FIXTURE = ROOT / "scripts" / "fixtures" / "linear_issue.json"
DEFAULT_WEBHOOK_FIXTURE = ROOT / "scripts" / "fixtures" / "linear_webhook_issue.json"


def find_free_port() -> int:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


def wait_for_port(port: int, timeout_secs: int) -> None:
    deadline = time.time() + timeout_secs
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                return
        except OSError:
            time.sleep(0.2)
    raise TimeoutError(f"Timed out waiting for port {port}")


def start_mock_linear_server(ledger_path: Path, issue_path: Path) -> tuple[subprocess.Popen, str]:
    cmd = [
        sys.executable,
        str(ROOT / "scripts" / "mock_linear_server.py"),
        "--ledger",
        str(ledger_path),
        "--issue",
        str(issue_path),
    ]
    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        cwd=str(ROOT),
    )
    assert proc.stdout is not None
    line = proc.stdout.readline().strip()
    if not line.startswith("listening="):
        stderr = proc.stderr.read() if proc.stderr else ""
        raise RuntimeError(f"Mock Linear server failed to start: {line}\n{stderr}")
    url = line.split("=", 1)[1]
    return proc, url


def build_config(path: Path, db_path: Path, api_base: str, webhook_port: int) -> None:
    content = f"""
[llm]
provider = "ollama"

[ollama]
url = "http://127.0.0.1:11434"

[db]
path = "{db_path}"

[ticket_intake]
enabled = false
poll_interval_secs = 300
mock_dispatch = true

[[ticket_intake.sources]]
provider = "linear"
repo = "ENG"
filter_label = "athena"
api_base = "{api_base}"
token_env = "LINEAR_API_KEY"

[ticket_intake.webhook]
enabled = true
bind = "127.0.0.1:{webhook_port}"
linear_secret_env = "LINEAR_WEBHOOK_SECRET"
""".lstrip()
    path.write_text(content)


def start_athena(config_path: Path, log_path: Path, athena_bin: str | None) -> subprocess.Popen:
    if athena_bin:
        cmd = [athena_bin, "--config", str(config_path), "chat"]
    else:
        local_bin = ROOT / "target" / "debug" / "athena"
        if local_bin.exists():
            cmd = [str(local_bin), "--config", str(config_path), "chat"]
        else:
            cmd = [
                "cargo",
                "run",
                "--quiet",
                "--features",
                "webhook",
                "--",
                "--config",
                str(config_path),
                "chat",
            ]

    log_file = log_path.open("w", encoding="utf-8")
    env = os.environ.copy()
    env["ATHENA_DISABLE_HOME_PROFILES"] = "1"
    env["ATHENA_SKIP_LLM_HEALTHCHECK"] = "1"
    env["LINEAR_API_KEY"] = "test"
    env["LINEAR_WEBHOOK_SECRET"] = "test-secret"

    return subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE,
        stdout=log_file,
        stderr=log_file,
        cwd=str(ROOT),
        env=env,
        text=True,
    )


def send_linear_webhook(port: int, payload_path: Path, secret: str) -> None:
    payload = json.loads(payload_path.read_text())
    body = json.dumps(payload).encode("utf-8")
    signature = hmac.new(secret.encode("utf-8"), body, sha256).hexdigest()

    url = f"http://127.0.0.1:{port}/webhook/linear"
    req = request.Request(
        url,
        data=body,
        headers={
            "Content-Type": "application/json",
            "Linear-Signature": signature,
        },
        method="POST",
    )
    with request.urlopen(req, timeout=5) as resp:  # noqa: S310
        if resp.status != 200:
            raise RuntimeError(f"Webhook returned {resp.status}")


def wait_for_writeback(ledger_path: Path, timeout_secs: int) -> List[Dict[str, Any]]:
    deadline = time.time() + timeout_secs
    while time.time() < deadline:
        if ledger_path.exists():
            events: List[Dict[str, Any]] = []
            for line in ledger_path.read_text().splitlines():
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
            has_comment = any(e.get("type") == "comment" for e in events)
            has_status = any(e.get("type") == "status" for e in events)
            if has_comment and has_status:
                return events
        time.sleep(1)
    raise TimeoutError("Timed out waiting for writeback events")


def check_ticket_status(db_path: Path) -> str | None:
    if not db_path.exists():
        return None
    conn = sqlite3.connect(str(db_path))
    try:
        row = conn.execute(
            "SELECT status FROM ticket_intake_log WHERE dedup_key = ?1",
            ("linear:ENG:issue-1",),
        ).fetchone()
        return row[0] if row else None
    finally:
        conn.close()


def wait_for_terminal_ticket_status(db_path: Path, timeout_secs: int) -> str | None:
    deadline = time.time() + timeout_secs
    last_status: str | None = None
    while time.time() < deadline:
        status = check_ticket_status(db_path)
        last_status = status
        if status in ("synced", "sync_failed"):
            return status
        time.sleep(1)
    return last_status


def terminate(proc: subprocess.Popen, name: str) -> None:
    if proc.poll() is not None:
        return
    try:
        proc.terminate()
        proc.wait(timeout=5)
    except Exception:
        try:
            proc.kill()
        except Exception:
            pass
        proc.wait(timeout=5)
    finally:
        if proc.poll() is None:
            raise RuntimeError(f"Failed to terminate {name}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--athena-bin", help="Path to athena binary (optional)")
    parser.add_argument("--timeout-secs", type=int, default=90)
    parser.add_argument("--sync-timeout-secs", type=int, default=45)
    args = parser.parse_args()

    temp_dir = Path.cwd() / ".context" / "user-flow"
    temp_dir.mkdir(parents=True, exist_ok=True)

    db_path = temp_dir / "athena.db"
    config_path = temp_dir / "config.toml"
    ledger_path = temp_dir / "linear_ledger.jsonl"
    athena_log = temp_dir / "athena.log"

    for path in (db_path, ledger_path, athena_log):
        if path.exists():
            path.unlink()

    webhook_port = find_free_port()

    mock_proc = None
    athena_proc = None

    try:
        mock_proc, linear_url = start_mock_linear_server(ledger_path, DEFAULT_ISSUE_FIXTURE)
        build_config(config_path, db_path, linear_url, webhook_port)

        athena_proc = start_athena(config_path, athena_log, args.athena_bin)
        wait_for_port(webhook_port, timeout_secs=args.timeout_secs)

        send_linear_webhook(webhook_port, DEFAULT_WEBHOOK_FIXTURE, "test-secret")
        wait_for_writeback(ledger_path, args.sync_timeout_secs)

        status = wait_for_terminal_ticket_status(db_path, args.sync_timeout_secs)
        if status not in ("synced", "sync_failed"):
            raise RuntimeError(f"Unexpected ticket status: {status}")

        print("user_flow=ok")
        print(f"ticket_status={status}")
        return 0
    except Exception as exc:
        print(f"user_flow=error reason={exc}")
        print(f"athena_log={athena_log}")
        return 1
    finally:
        if athena_proc is not None and athena_proc.stdin:
            try:
                athena_proc.stdin.write("/quit\n")
                athena_proc.stdin.flush()
            except Exception:
                pass
        if athena_proc is not None:
            terminate(athena_proc, "athena")
        if mock_proc is not None:
            terminate(mock_proc, "mock_linear_server")


if __name__ == "__main__":
    raise SystemExit(main())
