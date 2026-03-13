#!/usr/bin/env python3
"""Minimal Linear GraphQL stub for CI/user-flow harness.

Supports the subset used by Sparks's Linear provider:
- Issues query (poll)
- CommentCreate mutation (writeback)
- IssueState query (completed state lookup)
- IssueUpdate mutation (status update)

Writes writeback events to a JSONL ledger when --ledger is provided.
"""

from __future__ import annotations

import argparse
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from typing import Any, Dict

DEFAULT_ISSUE = {
    "id": "issue-1",
    "identifier": "ENG-1",
    "title": "Mock Linear issue",
    "description": "Mock issue body.",
    "url": "https://linear.app/ENG/issue/ENG-1",
    "priority": 3,
    "labels": ["sparks"],
    "creator": "Mock User",
}


def load_issue(path: str | None) -> Dict[str, Any]:
    if not path:
        return DEFAULT_ISSUE.copy()
    data = json.loads(Path(path).read_text())
    if not isinstance(data, dict):
        raise ValueError("Issue fixture must be a JSON object")
    return data


def issue_to_graphql(issue: Dict[str, Any]) -> Dict[str, Any]:
    labels = issue.get("labels") or []
    label_nodes = [{"name": name} for name in labels if isinstance(name, str)]
    creator_name = issue.get("creator")
    creator = {"name": creator_name} if creator_name else None
    return {
        "id": issue.get("id", ""),
        "identifier": issue.get("identifier"),
        "title": issue.get("title", "Untitled"),
        "description": issue.get("description"),
        "url": issue.get("url", ""),
        "priority": issue.get("priority"),
        "labels": {"nodes": label_nodes},
        "creator": creator,
    }


def append_ledger(ledger_path: str | None, event: Dict[str, Any]) -> None:
    if not ledger_path:
        return
    path = Path(ledger_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(event) + "\n")


class LinearHandler(BaseHTTPRequestHandler):
    server_version = "MockLinear/0.1"

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A003
        return

    def do_POST(self) -> None:  # noqa: N802
        if self.path.rstrip("/") != "/graphql":
            self.send_response(404)
            self.end_headers()
            return

        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        try:
            payload = json.loads(body)
        except json.JSONDecodeError:
            self.send_response(400)
            self.end_headers()
            self.wfile.write(b"Invalid JSON")
            return

        query = payload.get("query", "") or ""
        variables = payload.get("variables", {}) or {}

        if "Issues(" in query or "query Issues" in query:
            issue = issue_to_graphql(self.server.issue)
            resp = {"data": {"issues": {"nodes": [issue]}}}
            self._send_json(resp)
            return

        if "CommentCreate" in query or "commentCreate" in query:
            append_ledger(
                self.server.ledger_path,
                {
                    "type": "comment",
                    "issue_id": variables.get("issueId"),
                    "body": variables.get("body"),
                },
            )
            resp = {"data": {"commentCreate": {"success": True}}}
            self._send_json(resp)
            return

        if "IssueState" in query or "issueState" in query:
            issue_id = variables.get("id") or self.server.issue.get("id")
            resp = {
                "data": {
                    "issue": {
                        "id": issue_id,
                        "team": {
                            "states": {
                                "nodes": [
                                    {"id": "state-completed", "type": "completed", "name": "Completed"}
                                ]
                            }
                        },
                    }
                }
            }
            self._send_json(resp)
            return

        if "IssueUpdate" in query or "issueUpdate" in query:
            append_ledger(
                self.server.ledger_path,
                {
                    "type": "status",
                    "issue_id": variables.get("id"),
                    "state_id": variables.get("stateId"),
                },
            )
            resp = {"data": {"issueUpdate": {"success": True}}}
            self._send_json(resp)
            return

        self.send_response(400)
        self.end_headers()
        self.wfile.write(b"Unknown query")

    def _send_json(self, payload: Dict[str, Any]) -> None:
        data = json.dumps(payload).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class LinearServer(HTTPServer):
    def __init__(self, server_address, RequestHandlerClass, issue, ledger_path):
        super().__init__(server_address, RequestHandlerClass)
        self.issue = issue
        self.ledger_path = ledger_path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bind", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--issue", help="Path to issue JSON fixture")
    parser.add_argument("--ledger", help="Path to JSONL ledger for writeback events")
    args = parser.parse_args()

    try:
        issue = load_issue(args.issue)
    except Exception as exc:
        print(f"Failed to load issue fixture: {exc}", file=sys.stderr)
        return 2

    server = LinearServer((args.bind, args.port), LinearHandler, issue, args.ledger)
    host, port = server.server_address
    print(f"listening=http://{host}:{port}", flush=True)

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        return 0
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
