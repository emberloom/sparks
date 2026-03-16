#!/bin/bash
# Demo wrapper for VHS recording.
# Fakes all demo commands with pre-canned output for a clean recording.
#
# Usage: alias emberloom='bash scripts/demo-wrapper.sh'

p() {
  echo "$1"
  sleep "${2:-0.15}"
}

case "$1" in
  sparks)
    echo ""
    p "  Emberloom Sparks — active agents"
    echo ""
    p "  NAME       STRATEGY     TOOLS                           STATUS"
    p "  ─────────  ───────────  ──────────────────────────────  ──────"
    p "  scout      read-only    [glob, grep, read, git-log]     ready"
    p "  coder      autonomous   [read, edit, write, bash, git]  ready"
    p "  reviewer   advisory     [read, grep, git-diff]          ready"
    p "  ops        restricted   [bash, docker, systemctl]       ready"
    echo ""
    p "  4 sparks configured  ·  sandbox: docker (CAP_DROP ALL)"
    echo ""
    ;;
  dispatch)
    echo ""
    p "  classifying task..." 0.8
    p "  spark selected: coder  (confidence: 0.91)" 0.6
    p "  sandbox: starting container (CAP_DROP ALL, read-only rootfs)" 0.9
    p "  sandbox: ready" 0.5
    echo ""
    p "  [scout spark]  mapping repository... 47 files indexed" 0.5
    p "  [scout spark]  found entry point: src/main.rs" 0.3
    p "  [scout spark]  identified auth handler: src/handlers/auth.rs:142" 0.6
    echo ""
    p "  [coder spark]  reading src/handlers/auth.rs" 0.4
    p "  [coder spark]  reading src/config.rs" 0.3
    p "  [coder spark]  analysis: no rate limiting on POST /login — brute-force risk" 0.7
    p "  [coder spark]  plan: add token bucket per IP, configurable via config.toml" 0.8
    echo ""
    p "  [coder spark]  writing src/rate_limit.rs  (+89 lines)" 0.6
    p "  [coder spark]  patching src/handlers/auth.rs  (+12 lines, -2 lines)" 0.4
    p "  [coder spark]  patching src/config.rs  (+8 lines)" 0.4
    p "  [coder spark]  running: cargo test" 0.4
    p "                 test rate_limit::tests::bucket_refills ... ok" 0.25
    p "                 test rate_limit::tests::blocks_after_limit ... ok" 0.25
    p "                 test auth::tests::login_success ... ok" 0.25
    p "                 test auth::tests::login_rate_limited ... ok" 0.25
    p "                 test result: ok. 288 passed; 0 failed" 0.6
    echo ""
    p "  [coder spark]  committing: \"feat: add per-IP rate limiting to login endpoint\"" 0.5
    echo ""
    p "  task complete  ·  elapsed: 54s  ·  tools: 8  ·  status: success" 0.2
    echo ""
    ;;
  kpi)
    echo ""
    p "  Emberloom KPI Dashboard"
    echo ""
    p "  METRIC                 VALUE     TREND"
    p "  ─────────────────────  ────────  ─────"
    p "  tasks completed        142       ▲ +12"
    p "  success rate            94.4%    ▲ +1.2%"
    p "  avg completion time     48s      ▼ -6s"
    p "  tests passing           100%     ━"
    p "  memory entries          1,847    ▲ +89"
    p "  sparks active           4/4      ━"
    echo ""
    p "  last task: 2m ago  ·  uptime: 4h 22m"
    echo ""
    ;;
  *)
    echo "  emberloom: unknown command '$1'"
    echo "  usage: emberloom {sparks|dispatch|kpi}"
    ;;
esac
