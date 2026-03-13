#!/bin/bash
# Demo wrapper for VHS recording.
# Passes all commands to the real sparks binary EXCEPT `dispatch`,
# which prints pre-canned output line-by-line with delays.
#
# Usage: alias sparks='bash scripts/demo-wrapper.sh'

REAL_SPARKS="cargo run --quiet --"

p() {
  echo "$1"
  sleep "${2:-0.15}"
}

case "$1" in
  dispatch)
    echo ""
    p "  classifying task..." 0.8
    p "  ghost selected: coder  (confidence: 0.91)" 0.6
    p "  sandbox: starting container (CAP_DROP ALL, read-only rootfs)" 0.9
    p "  sandbox: ready" 0.5
    echo ""
    p "  [scout]  mapping repository... 47 files indexed" 0.5
    p "  [scout]  found entry point: src/main.rs" 0.3
    p "  [scout]  identified auth handler: src/handlers/auth.rs:142" 0.6
    echo ""
    p "  [coder]  reading src/handlers/auth.rs" 0.4
    p "  [coder]  reading src/config.rs" 0.3
    p "  [coder]  analysis: no rate limiting on POST /login — brute-force risk" 0.7
    p "  [coder]  plan: add token bucket per IP, configurable via config.toml" 0.8
    echo ""
    p "  [coder]  writing src/rate_limit.rs  (+89 lines)" 0.6
    p "  [coder]  patching src/handlers/auth.rs  (+12 lines, -2 lines)" 0.4
    p "  [coder]  patching src/config.rs  (+8 lines)" 0.4
    p "  [coder]  running: cargo test" 0.4
    p "           test rate_limit::tests::bucket_refills ... ok" 0.25
    p "           test rate_limit::tests::blocks_after_limit ... ok" 0.25
    p "           test auth::tests::login_success ... ok" 0.25
    p "           test auth::tests::login_rate_limited ... ok" 0.25
    p "           test result: ok. 288 passed; 0 failed" 0.6
    echo ""
    p "  [coder]  committing: \"feat: add per-IP rate limiting to login endpoint\"" 0.5
    echo ""
    p "  task complete  ·  elapsed: 54s  ·  tools: 8  ·  status: success" 0.2
    echo ""
    ;;
  *)
    $REAL_SPARKS "$@" 2>/dev/null
    ;;
esac
