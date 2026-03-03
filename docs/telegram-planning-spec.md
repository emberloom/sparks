# Telegram Planning Interview Spec

## Overview
The Telegram planning interview is a lightweight, low-friction flow that captures the minimum inputs needed to produce a high-quality plan. It is optimized for 2–4 turns, uses quick-reply inline keyboards where possible, and falls back to free text when needed.

## Goals
- Capture success criteria, constraints, and output format with minimal back-and-forth.
- Make planning feel guided and intentional, not bureaucratic.
- Provide clear, staged loading states once plan generation begins.
- Offer a simple edit loop before plan generation.

## Non-goals
- Full natural language slot-filling.
- Multi-branch, form-like data entry.
- Replacing the core planner or LLM behavior.

## Entry Conditions
- Explicit: `/plan` command.
- Automatic: message contains planning keywords such as `plan`, `planning`, `roadmap`, `strategy`, `launch`, `rollout`, `gtm`.

## State Machine
- `Goal` → `Constraints` → `Output` → `Summary` → `Generate`
- Optional: `Editing` (if the user wants changes after summary).

## Data Captured
- `goal` (required)
- `constraints` (optional)
- `timeline` (optional)
- `scope` (optional)
- `depth` (optional; quick/standard/deep)
- `output` (optional; checklist/spec/draft)

## Prompts
- Goal: “What does success look like? One sentence is fine.”
- Constraints: “Any constraints I should respect? (timeline, budget, stack, scope)”
- Output: “What format do you want the plan in: checklist, spec, or narrative?”
- Summary: bullets with Goal, Constraints, Timeline, Scope, Depth, Output.
- Edit: “Send corrections using lines like `Goal: ...` / `Constraints: ...`.”

## Quick Reply Buttons
- Depth: `Quick`, `Standard`, `Deep`
- Timeline: `Today`, `This week`, `No deadline`
- Scope: `Idea only`, `Implementation`, `Full plan`
- Constraints: `No constraints`, `Skip`
- Output: `Checklist`, `Spec`, `Draft`
- Summary: `Confirm`, `Edit`, `Cancel`

## Callback Data Format
- `plan:depth:quick`
- `plan:timeline:week`
- `plan:scope:impl`
- `plan:constraints:none`
- `plan:output:spec`
- `plan:confirm:yes`

## Defaults
- Output format defaults to `checklist` if skipped.
- Depth defaults to `standard` if unspecified.

## Cancellation
- User can cancel via `/plan cancel` or by typing `cancel`.

## Loading States
- Initial status message: “Status: Starting...”
- During planning generation: “Status: Drafting plan...”
- Streaming statuses from the core are prefixed as “Status: ...”
- Tool runs appear as “Behind the scenes — {tool}” blocks.

## Telemetry (Suggested)
- `planning_start`
- `planning_question_asked`
- `planning_question_answered`
- `planning_summary_shown`
- `planning_confirmed`
- `planning_plan_generated`
- `planning_abandoned`

## Config
- `planning_enabled` (default: true)
- `planning_auto` (default: true)
- `planning_timeout_secs` (default: 900)

## Failure Modes
- Stale interview session: user is prompted to restart.
- Unknown edit format: user is re-prompted with examples.
- Core failure: status message shows an error and interview ends.
