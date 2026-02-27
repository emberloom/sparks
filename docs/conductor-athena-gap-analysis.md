# Conductor vs Athena: Gap Analysis

**Date**: 2026-02-27
**Purpose**: Identify features from [Conductor](https://www.conductor.build) that Athena should adopt to strengthen its multi-agent orchestration capabilities.

---

## Executive Summary

Conductor is a macOS desktop application for running multiple AI coding agents (Claude Code, Codex) in parallel with isolated git worktrees, visual monitoring, and diff-first code review. Athena is a Rust-based autonomous multi-agent system with deep self-improvement, memory, scheduling, and evaluation capabilities.

While Athena excels in areas Conductor does not attempt (self-improvement funnels, semantic memory, mood/proactive behavior, KPI tracking, eval harnesses, bounded autonomy), Conductor has several **developer-experience and orchestration features** that would significantly strengthen Athena's usability and multi-agent workflow capabilities.

---

## Feature Comparison Matrix

| Category | Feature | Conductor | Athena | Gap? |
|----------|---------|-----------|--------|------|
| **Parallel Execution** | Multiple agents in parallel | Yes (unlimited workspaces) | Partial (async tasks, Docker) | **YES** |
| **Workspace Isolation** | Git worktree per agent | Yes (automatic) | No (Docker-based only) | **YES** |
| **Visual Dashboard** | GUI for monitoring agents | Yes (native macOS app) | No (CLI + observer socket) | **YES** |
| **Diff Review** | Integrated diff viewer | Yes (diff-first review) | No | **YES** |
| **Checkpoints** | Revert to previous turns | Yes | No | **YES** |
| **GitHub Integration** | PR creation/comments/actions | Deep (sync, actions, comments) | Basic (gh CLI tool) | **YES** |
| **Linear Integration** | Issue linking | Yes | No | **YES** |
| **MCP Support** | Model Context Protocol | Yes | No | **YES** |
| **IDE Integration** | Open files in editors | Yes (10+ editors) | No | **YES** |
| **Planning Mode** | Interactive plan review | Yes (plan + approve) | Partial (feature contracts) | **YES** |
| **Workspace Lifecycle** | Create from PR/branch/issue | Yes | No | **YES** |
| **Setup Scripts** | Automated env initialization | Yes (setup/run/archive) | No | **YES** |
| **Queue System** | Ordered message processing | Yes | No | **YES** |
| **Cost Tracking** | Per-response token/cost | Yes | Partial (token counts) | **PARTIAL** |
| **Multi-Repo** | Cross-repo orchestration | Yes | No | **YES** |
| **File Browser** | File tree + fuzzy search | Yes | No | **YES** |
| **Notes/Scratchpad** | Workspace notes | Yes | No | **YES** |
| **Todo System** | User-facing task tracking | Yes (blocks merging) | No (internal only) | **YES** |
| **Slash Commands** | Custom user commands | Yes | No | **YES** |
| **Agent Handoff** | Pass plans between agents | Yes | No | **YES** |
| **Browser Testing** | Screenshot/localhost testing | Yes | No | **YES** |
| **Code Review Tool** | Customizable review prompts | Yes | No | **YES** |
| **Memory System** | Semantic long-term memory | No | Yes | Athena leads |
| **Self-Improvement** | Autonomous code evolution | No | Yes (4 funnels) | Athena leads |
| **Eval Harness** | Benchmark evaluation | No | Yes | Athena leads |
| **KPI Tracking** | Mission metrics | No | Yes | Athena leads |
| **Mood/Proactive** | Personality + initiative | No | Yes | Athena leads |
| **Scheduling** | Cron/interval jobs | No | Yes | Athena leads |
| **Bounded Autonomy** | 5-level autonomy ladder | No | Yes | Athena leads |
| **Self-Healing** | Auto-fix tool/test failures | No | Yes | Athena leads |

---

## Priority 1: Critical Gaps (High Impact, Core Orchestration)

### 1.1 Git Worktree-Based Workspace Isolation

**What Conductor does**: Each agent workspace is an isolated git worktree. Agents cannot interfere with each other. Worktrees share `.git` but have separate working trees.

**What Athena has**: Docker-based execution with mounted workspaces. No native git worktree isolation for parallel agent work on the same repo.

**Recommendation**: Implement a `Workspace` abstraction that creates git worktrees for each ghost/agent task. This enables:
- True parallel execution on the same repo without conflicts
- Lightweight isolation (no Docker overhead for branch-level work)
- Easy merging of agent results back to the target branch
- Workspace lifecycle: create -> work -> review -> merge -> archive

**Implementation sketch**:
- `workspace.rs` module managing git worktree lifecycle
- `git worktree add` / `git worktree remove` wrappers
- Workspace registry in SQLite tracking active/archived worktrees
- Optional setup scripts per workspace (copy .env, install deps)

---

### 1.2 Parallel Multi-Agent Orchestration with Monitoring

**What Conductor does**: Spin up N agents simultaneously, each in isolated workspaces, with a unified dashboard showing status, progress, and which files each agent is modifying.

**What Athena has**: Async task dispatch and Docker execution, but no unified view of multiple concurrent agents working on the same project.

**Recommendation**: Enhance the task dispatch system to support true parallel multi-agent workflows:
- Parallel task batch submission (dispatch N tasks simultaneously)
- Real-time status aggregation across active agents
- Observer event enrichment with workspace/agent attribution
- `athena parallel` CLI command for launching coordinated multi-agent work
- Progress summary showing: agent name, workspace, current phase, files modified

---

### 1.3 Diff-First Review Workflow

**What Conductor does**: After an agent completes work, changes are presented as a clean diff. Users review only what changed, accept/reject changes, and merge.

**What Athena has**: No integrated review step. Agents execute and produce outcomes, but there's no structured review-before-merge gate.

**Recommendation**: Add a review phase to the task/feature contract lifecycle:
- After agent execution, generate a structured diff summary
- `athena review <workspace>` command showing file-by-file changes
- Accept/reject per file or per hunk
- Integration with the existing feature contract verify/promote pipeline
- Optional: LLM-powered review that summarizes what changed and flags concerns

---

### 1.4 Checkpoints and Rollback

**What Conductor does**: Turn-by-turn checkpoints let users revert to any previous point in an agent's work. Full history of changes with restore capability.

**What Athena has**: No checkpoint system. Task outcomes are recorded but workspace state is not snapshotted.

**Recommendation**: Implement checkpoint tracking for agent work:
- Git commit after each significant agent step (explore, execute, verify)
- Checkpoint registry mapping step -> commit SHA
- `athena checkpoint list/restore <workspace>` commands
- Integration with worktree system for clean state management

---

## Priority 2: High-Value Gaps (Developer Experience)

### 2.1 GitHub Integration Depth

**What Conductor does**: Create workspaces from PRs, sync PR comments to agent chat, view GitHub Actions logs, re-run failed checks, auto-create PRs with status tracking.

**What Athena has**: Basic `gh` CLI tool access. No PR comment sync, no Actions integration, no workspace-from-PR creation.

**Recommendation**: Build a `github.rs` integration module:
- `athena workspace from-pr <url>` to create a worktree from a PR
- PR comment sync: fetch comments, feed to agent as context
- GitHub Actions status polling and log retrieval
- Auto-PR creation after successful feature contract completion
- Webhook receiver for real-time PR event processing

---

### 2.2 MCP (Model Context Protocol) Support

**What Conductor does**: Supports MCP servers via `.mcp.json` configuration. All worktrees inherit MCP configuration, enabling agents to access external tools and data sources.

**What Athena has**: Dynamic tool discovery from filesystem, but no MCP protocol support.

**Recommendation**: Implement MCP client support:
- Parse `.mcp.json` configuration files
- MCP server connection management
- Expose MCP tools alongside native tools in the tool registry
- Enable ghosts to call MCP-provided tools seamlessly
- This unlocks access to the entire MCP ecosystem (databases, APIs, browsers, etc.)

---

### 2.3 Interactive Planning Mode

**What Conductor does**: Plan mode where the agent proposes a plan, user reviews and provides feedback, plan gets refined before execution begins. Plans can be handed off between agents.

**What Athena has**: Feature contracts with task DAGs, but no interactive plan-review-approve cycle with user feedback.

**Recommendation**: Add an interactive planning phase:
- `athena plan <goal>` generates a structured execution plan
- Plan presented for user review with accept/modify/reject options
- Feedback loop: user comments -> agent refines plan
- Approved plan becomes a feature contract for execution
- Support plan handoff: one ghost plans, another executes

---

### 2.4 Workspace Lifecycle Scripts

**What Conductor does**: Setup scripts (run on workspace creation: copy .env, install deps), run scripts (dev servers, tests), and archive scripts (cleanup after merge).

**What Athena has**: No scripted workspace lifecycle.

**Recommendation**: Add configurable lifecycle hooks:
- `.athena/setup.sh` - runs when a workspace/worktree is created
- `.athena/run.sh` - starts development server or test runner
- `.athena/teardown.sh` - cleanup after workspace is archived
- Scripts defined per-repo in `.athena/config.toml`
- Support for tool version managers (mise, asdf, nvm)

---

### 2.5 Task Queue System

**What Conductor does**: Message queue that processes multiple tasks in order, allowing users to queue up work items that agents process sequentially.

**What Athena has**: Async task dispatch with scheduled jobs, but no ordered queue for user-submitted work items.

**Recommendation**: Implement a task queue:
- `athena queue add "implement feature X"` adds work to the queue
- FIFO processing with optional priority levels
- Queue status visibility: pending, active, completed
- Pause/resume queue processing
- Integration with workspace system (each queue item gets a workspace)

---

### 2.6 Custom Slash Commands

**What Conductor does**: Users define custom slash commands that capture frequent actions into short commands. Project-level and global slash command support.

**What Athena has**: CLI subcommands but no user-definable shortcut commands.

**Recommendation**: Add a slash command system:
- `.athena/commands/` directory with TOML/YAML command definitions
- Each command: name, description, prompt template, optional tool chain
- `athena run /<command> [args]` execution
- Built-in commands: `/review`, `/test`, `/fix`, `/deploy`
- Commands shareable via conductor.json-like config

---

## Priority 3: Valuable Gaps (Enhanced Capabilities)

### 3.1 Agent-to-Agent Handoff

**What Conductor does**: Plans can be handed off between agents. One agent creates a plan, another executes it.

**What Athena has**: Ghost routing in manager, but no explicit inter-agent handoff protocol.

**Recommendation**: Implement structured handoff:
- Handoff envelope: context summary, plan, relevant files, constraints
- `manager.rs` handoff routing: ghost A -> ghost B with preserved context
- Support handoff chains: scout -> planner -> coder -> reviewer
- Handoff triggers: automatic (on phase completion) or manual

---

### 3.2 Cost Tracking and Budget Controls

**What Conductor does**: Per-response token count and cost display. Users see exactly what each agent interaction costs.

**What Athena has**: Token usage tracking (call counts, latency), but no per-request cost calculation or budget enforcement.

**Recommendation**: Enhance cost visibility:
- Map token counts to provider-specific pricing
- Per-task and per-workspace cost accumulation
- Cost display in observer events and CLI output
- Optional budget caps: per-task, per-workspace, per-day
- Cost reporting in KPI snapshots

---

### 3.3 Multi-Repository Orchestration

**What Conductor does**: Group workspaces by repository. Work across multiple repos simultaneously.

**What Athena has**: Single-repo focus per task.

**Recommendation**: Add multi-repo awareness:
- Task context can reference multiple repos
- Workspace creation across repo boundaries
- Cross-repo dependency tracking in feature contracts
- Useful for monorepo-adjacent and microservice architectures

---

### 3.4 Browser-Based Testing

**What Conductor does**: Localhost URL detection, browser screenshots, Chrome integration for visual testing.

**What Athena has**: No browser integration.

**Recommendation**: Add browser testing capability:
- Detect localhost URLs in agent output
- Headless Chrome/Playwright integration for screenshots
- Visual diff for UI changes
- Screenshot tool available to ghosts for verification

---

### 3.5 Code Review Tool

**What Conductor does**: Customizable code review with configurable prompts. Review button triggers LLM-powered analysis of changes.

**What Athena has**: Eval harness and quality gates, but no dedicated code review step.

**Recommendation**: Add an LLM-powered review ghost:
- Dedicated "reviewer" ghost personality focused on code quality
- Configurable review criteria (security, performance, style, correctness)
- Review runs automatically after execution phase
- Findings formatted as actionable feedback
- Integration with real-gate for PR-blocking reviews

---

### 3.6 Notes and Context Sharing

**What Conductor does**: Workspace notes/scratchpad with markdown preview. `.context` directory for shared attachments and notes across workspaces.

**What Athena has**: Memory system (long-term), but no per-workspace scratchpad or shared context directory.

**Recommendation**: Add workspace-scoped context:
- Per-workspace notes stored alongside worktree
- `.athena/context/` directory for shared files across agents
- Notes auto-injected into agent system prompt
- Markdown support with structured sections

---

## Priority 4: Nice-to-Have Gaps

### 4.1 Visual Dashboard / TUI

While Athena's CLI + observer socket is powerful, a TUI (terminal UI) would provide Conductor-like visibility without requiring a full GUI:
- `athena dashboard` using ratatui or similar
- Real-time agent status, workspace list, diff preview
- Keyboard-driven navigation

### 4.2 IDE Integration

- `athena open <file>:<line>` command to open files in user's preferred editor
- Editor detection and configuration
- Deep-link support for external tools (Slack, Linear, GitHub)

### 4.3 File Browser and Picker

- Interactive file selection for agent context
- Fuzzy search across workspace files
- File mention system in task descriptions

### 4.4 Linear/Issue Tracker Integration

- Create workspaces from Linear/Jira/GitHub issues
- Status sync between Athena tasks and external trackers
- Auto-update issue status on task completion

---

## Implementation Roadmap

### Phase 1: Foundation (Weeks 1-3)
1. **Git worktree workspace system** (1.1) - This is the foundational primitive
2. **Checkpoint system** (1.4) - Built on top of worktrees
3. **Workspace lifecycle scripts** (2.4) - Essential for usable worktrees

### Phase 2: Orchestration (Weeks 3-5)
4. **Parallel multi-agent orchestration** (1.2) - Leverage worktrees for true parallelism
5. **Task queue system** (2.5) - Ordered work processing
6. **Agent-to-agent handoff** (3.1) - Structured multi-agent pipelines

### Phase 3: Review & Integration (Weeks 5-7)
7. **Diff-first review workflow** (1.3) - Review gate before merging
8. **GitHub integration depth** (2.1) - PR-centric workflows
9. **Code review tool** (3.5) - Automated review ghost

### Phase 4: Developer Experience (Weeks 7-9)
10. **Interactive planning mode** (2.3) - Plan-review-execute cycle
11. **Custom slash commands** (2.6) - User-definable shortcuts
12. **MCP support** (2.2) - External tool ecosystem access
13. **Cost tracking** (3.2) - Budget visibility and controls

### Phase 5: Polish (Weeks 9-12)
14. **TUI dashboard** (4.1) - Visual monitoring
15. **Multi-repo support** (3.3) - Cross-repo orchestration
16. **Browser testing** (3.4) - Visual verification
17. **Notes/context sharing** (3.6) - Workspace scratchpad
18. **IDE integration** (4.2) - Editor deep-links

---

## Summary of Top 5 Recommendations

| # | Feature | Why It Matters |
|---|---------|----------------|
| 1 | **Git Worktree Workspaces** | Foundational primitive for parallel agent isolation without Docker overhead |
| 2 | **Diff-First Review Workflow** | Critical missing gate between agent execution and code integration |
| 3 | **Parallel Multi-Agent Monitoring** | Users need visibility when running multiple agents simultaneously |
| 4 | **MCP Support** | Unlocks entire ecosystem of external tools and data sources |
| 5 | **Interactive Planning Mode** | Human-in-the-loop plan review prevents wasted agent effort |

---

## What Athena Does Better Than Conductor

Athena has significant advantages that should be preserved and leveraged:

- **Semantic Memory**: Long-term learning across sessions (Conductor has none)
- **Self-Improvement Funnels**: Autonomous code evolution, health monitoring, refactoring
- **Bounded Autonomy Ladder**: 5-level safety model for autonomous actions
- **Eval Harness**: Systematic benchmarking of agent performance
- **KPI Tracking**: Mission-driven metrics with lane/repo/risk segmentation
- **Proactive Behavior**: Heartbeat, idle musings, conversation re-entry
- **Mood System**: Personality modeling for natural interaction
- **Self-Healing**: Automatic recovery from tool and test failures
- **Scheduled Jobs**: Cron/interval-based autonomous work
- **Execution Contracts**: Deterministic error taxonomy with retry/fallback policies

The goal is not to become Conductor, but to adopt its best orchestration patterns while maintaining Athena's unique depth in autonomy, learning, and self-improvement.
