#!/usr/bin/env python3
"""
SonarQube quality gate poller for Sparks CI pipeline.

Usage:
    python3 scripts/sonarqube_gate.py [--project-key KEY] [--timeout 120] [--poll 5]

Environment variables:
    SONAR_HOST_URL        SonarQube server URL (default: https://sonarcloud.io)
    SONAR_PROJECT_KEY     Project key
    SONAR_TOKEN           Authentication token (also accepted as SPARKS_SONAR_TOKEN)
    SONAR_ORGANIZATION    Organisation key (required for SonarCloud)
"""
import argparse
import base64
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Optional


def get_gate_status(
    server_url: str,
    project_key: str,
    token: Optional[str],
    organization: Optional[str],
) -> dict:
    url = f"{server_url.rstrip('/')}/api/qualitygates/project_status"
    params: dict[str, str] = {"projectKey": project_key}
    if organization:
        params["organization"] = organization
    url += "?" + urllib.parse.urlencode(params)

    req = urllib.request.Request(url)
    if token:
        creds = base64.b64encode(f"{token}:".encode()).decode()
        req.add_header("Authorization", f"Basic {creds}")

    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        body = e.read().decode()
        print(f"SonarQube API error {e.code}: {body}", file=sys.stderr)
        sys.exit(1)


def main() -> None:
    parser = argparse.ArgumentParser(description="Poll SonarQube quality gate")
    parser.add_argument(
        "--server",
        default=os.environ.get("SONAR_HOST_URL", "https://sonarcloud.io"),
        help="SonarQube server URL",
    )
    parser.add_argument(
        "--project-key",
        default=os.environ.get("SONAR_PROJECT_KEY"),
        help="SonarQube project key",
    )
    parser.add_argument(
        "--token",
        default=os.environ.get("SONAR_TOKEN") or os.environ.get("SPARKS_SONAR_TOKEN"),
        help="Authentication token",
    )
    parser.add_argument(
        "--organization",
        default=os.environ.get("SONAR_ORGANIZATION"),
        help="Organisation key (SonarCloud only)",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=120,
        help="Maximum seconds to wait for gate (default: 120)",
    )
    parser.add_argument(
        "--poll",
        type=int,
        default=5,
        help="Poll interval in seconds (default: 5)",
    )
    parser.add_argument(
        "--allow-warn",
        action="store_true",
        help="Treat WARN as passing (default: WARN is already passing)",
    )
    args = parser.parse_args()

    if not args.project_key:
        print(
            "Error: --project-key or SONAR_PROJECT_KEY env var required",
            file=sys.stderr,
        )
        sys.exit(1)

    deadline = time.time() + args.timeout
    attempts = 0

    while True:
        attempts += 1
        data = get_gate_status(args.server, args.project_key, args.token, args.organization)
        status = data["projectStatus"]["status"]
        conditions = data["projectStatus"].get("conditions", [])

        if status in ("OK", "WARN"):
            print(f"Quality gate: {status} [PASS]")
            sys.exit(0)

        if status == "ERROR":
            print("Quality gate: FAILED")
            failed = [c for c in conditions if c["status"] == "ERROR"]
            for c in failed:
                metric = c["metricKey"]
                actual = c.get("actualValue", "?")
                threshold = c.get("errorThreshold", "?")
                print(f"   - {metric} = {actual} (threshold: {threshold})")
            sys.exit(1)

        if status == "NONE":
            print("Quality gate: NONE (no analysis found)", file=sys.stderr)
            sys.exit(1)

        # PENDING or IN_PROGRESS — keep polling
        if time.time() >= deadline:
            print(
                f"Quality gate timed out after {args.timeout}s (status: {status})",
                file=sys.stderr,
            )
            sys.exit(1)

        print(f"Quality gate: {status} (attempt {attempts}, polling again in {args.poll}s...)")
        time.sleep(args.poll)


if __name__ == "__main__":
    main()
