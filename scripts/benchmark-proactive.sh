#!/usr/bin/env bash
# benchmark-proactive.sh — Collect observer events and print a scorecard.
# Usage: ./scripts/benchmark-proactive.sh [duration_minutes]
#
# Connects to the observer UDS socket, collects JSON events for the
# specified duration (default: 10 min), then computes metrics.
set -euo pipefail

DURATION_MIN="${1:-10}"
DURATION_SEC=$((DURATION_MIN * 60))
SOCKET="${ATHENA_OBSERVER_SOCK:-$HOME/.athena/observer.sock}"
TMPFILE=$(mktemp /tmp/athena-bench.XXXXXX)
trap 'rm -f "$TMPFILE"' EXIT

if [ ! -S "$SOCKET" ]; then
  echo "Error: observer socket not found at $SOCKET"
  echo "Make sure Athena is running with 'athena observe' or set ATHENA_OBSERVER_SOCK."
  exit 1
fi

echo "Collecting events for ${DURATION_MIN}m from $SOCKET ..."
echo "(Press Ctrl-C to stop early)"

# Collect events with timeout (portable — works on macOS without coreutils)
nc -U "$SOCKET" > "$TMPFILE" 2>/dev/null &
NC_PID=$!
sleep "$DURATION_SEC"
kill "$NC_PID" 2>/dev/null
wait "$NC_PID" 2>/dev/null || true

TOTAL=$(wc -l < "$TMPFILE" | tr -d ' ')

if [ "$TOTAL" -eq 0 ]; then
  echo "No events collected. Is the observer producing output?"
  exit 1
fi

echo ""
echo "═══════════════════════════════════════════════════"
echo "  Athena Proactive Benchmark — ${DURATION_MIN}m collection"
echo "═══════════════════════════════════════════════════"
echo ""

# Count events by category
echo "── Event Counts ──"
# Categories in JSON match Rust enum variants (e.g. "Heartbeat", "MoodChange")
if command -v jq &>/dev/null; then
  # Use jq for robust JSON parsing
  jq -r '.category // "unknown"' "$TMPFILE" 2>/dev/null | sort | uniq -c | sort -rn | while read -r count cat; do
    printf "  %-20s %d\n" "$cat" "$count"
  done

  count_cat() { jq -r "select(.category == \"$1\") | .category" "$TMPFILE" 2>/dev/null | wc -l | tr -d ' '; }
  count_msg() { jq -r "select(.message | test(\"$1\"; \"i\")) | .message" "$TMPFILE" 2>/dev/null | wc -l | tr -d ' '; }

  HEARTBEAT=$(count_cat Heartbeat)
  MOOD=$(count_cat MoodChange)
  ENERGY=$(count_cat EnergyShift)
  MEMORY_SCAN=$(count_cat MemoryScan)
  IDLE=$(count_cat IdleMusing)
  CRON=$(count_cat CronTick)
  STOCHASTIC=$(count_cat StochasticRoll)
  PULSE_EMIT=$(count_cat PulseEmitted)
  PULSE_SUPP=$(count_cat PulseSuppressed)
  PULSE_DELIV=$(count_cat PulseDelivered)
  DELIVERED=$PULSE_DELIV
  SUPPRESSED=$((PULSE_SUPP + $(count_msg "suppressed by gate")))
else
  # Fallback: grep-based parsing
  grep -o '"category":"[^"]*"' "$TMPFILE" | sort | uniq -c | sort -rn | while read -r count cat; do
    printf "  %-20s %d\n" "$cat" "$count"
  done

  count_cat() { grep -c "\"$1\"" "$TMPFILE" 2>/dev/null || echo 0; }
  HEARTBEAT=$(count_cat Heartbeat)
  MOOD=$(count_cat MoodChange)
  ENERGY=$(count_cat EnergyShift)
  MEMORY_SCAN=$(count_cat MemoryScan)
  IDLE=$(count_cat IdleMusing)
  CRON=$(count_cat CronTick)
  STOCHASTIC=$(count_cat StochasticRoll)
  PULSE_EMIT=$(count_cat PulseEmitted)
  PULSE_SUPP=$(count_cat PulseSuppressed)
  PULSE_DELIV=$(count_cat PulseDelivered)
  DELIVERED=$PULSE_DELIV
  SUPPRESSED=$((PULSE_SUPP + $(grep -ci "suppressed by gate" "$TMPFILE" 2>/dev/null || echo 0)))
fi

EMITTED=$((DELIVERED + SUPPRESSED))

echo ""
echo "── Metrics ──"

# Event density
HOURS=$(echo "scale=4; $DURATION_MIN / 60" | bc)
DENSITY=$(echo "scale=1; $TOTAL / $HOURS" | bc)
echo -n "  Event density:        ${DENSITY}/hr"
if (( $(echo "$DENSITY >= 5 && $DENSITY <= 20" | bc -l) )); then
  echo "  [GOOD: 5-20/hr]"
else
  echo "  [WARNING: outside 5-20/hr]"
fi

# Pulse delivery rate
if [ "$EMITTED" -gt 0 ]; then
  DELIVERY_RATE=$(echo "scale=2; $DELIVERED / $EMITTED" | bc)
  echo -n "  Pulse delivery rate:  ${DELIVERY_RATE}"
  if (( $(echo "$DELIVERY_RATE >= 0.3 && $DELIVERY_RATE <= 0.7" | bc -l) )); then
    echo "  [GOOD: 0.3-0.7]"
  else
    echo "  [WARNING: outside 0.3-0.7]"
  fi

  SUPPRESS_RATE=$(echo "scale=2; $SUPPRESSED / $EMITTED" | bc)
  echo -n "  Suppression rate:     ${SUPPRESS_RATE}"
  if (( $(echo "$SUPPRESS_RATE >= 0.3 && $SUPPRESS_RATE <= 0.7" | bc -l) )); then
    echo "  [GOOD: 0.3-0.7]"
  else
    echo "  [WARNING: outside 0.3-0.7]"
  fi
else
  echo "  Pulse delivery rate:  N/A (no pulses emitted)"
  echo "  Suppression rate:     N/A (no pulses emitted)"
fi

# Cron noise
if [ "$TOTAL" -gt 0 ]; then
  CRON_RATIO=$(echo "scale=3; $CRON / $TOTAL" | bc)
  echo -n "  Cron noise:           ${CRON_RATIO}"
  if (( $(echo "$CRON_RATIO < 0.05" | bc -l) )); then
    echo "  [GOOD: <0.05]"
  else
    echo "  [WARNING: >= 0.05]"
  fi
fi

# Feedback rate (estimate from stored categories)
LLM_CALLS=$((HEARTBEAT + MEMORY_SCAN + IDLE))
STORED=$((STOCHASTIC > 0 ? STOCHASTIC : 0))  # approximation
if [ "$LLM_CALLS" -gt 0 ]; then
  echo "  LLM-triggered events: ${LLM_CALLS}"
fi

echo ""
echo "── Summary ──"
echo "  Total events:   $TOTAL"
echo "  Duration:       ${DURATION_MIN}m"
echo "  Heartbeats:     $HEARTBEAT"
echo "  Mood changes:   $MOOD"
echo "  Energy drifts:  $ENERGY"
echo "  Memory scans:   $MEMORY_SCAN"
echo "  Idle musings:   $IDLE"
echo "  Cron events:    $CRON"
echo "  Stochastic:     $STOCHASTIC"
echo "  Pulse emitted:  $PULSE_EMIT"
echo "  Pulse suppressed: $PULSE_SUPP"
echo "  Pulse delivered: $PULSE_DELIV"
echo ""
echo "Raw events saved to: $TMPFILE"
trap - EXIT  # keep the file for inspection
echo "Done."
