# Athena Self-Improvement Architecture

> Systems that think, adapt, and act with bounded autonomy.

## System Overview

## Target Operating Model (Spec-Driven)

Athena's long-horizon engineering loop should follow one explicit contract stack:

1. `Feature Contract` defines user outcome, scope, constraints, and acceptance criteria.
2. `Task Contracts` decompose a feature into a DAG of atomic tasks with dependencies.
3. `Execution Contract` runs each task through normalized CLI wrappers and deterministic retry/fallback policy.
4. `Eval Gate` scores plan/execution/tests/diff and blocks weak outcomes.
5. `Promotion Policy` applies risk-tier controls (low-risk auto-merge only; medium/high risk PR-only).

Key references:

- `docs/feature-contract-v1.md`
- `docs/task-contract-v1.md`
- `docs/execution-contract-v1.md`
- `docs/mission-contract.md`
- `docs/self-improvement-roadmap.md`

### Feature to Task DAG Scaling

To scale from task-level execution to feature-level delivery:

- each feature owns a single acceptance criteria set with stable IDs (for example, `AC-1`, `AC-2`)
- each task maps to at least one acceptance criterion
- dependencies are represented as a DAG (no cycles), so independent tasks can run in parallel
- feature completion is blocked until all acceptance IDs have passing evidence

```mermaid
flowchart LR
    FC["Feature Contract"] --> TD["Task DAG"]
    TD --> T1["T1: Interface Contract"]
    TD --> T2["T2: Backend Implementation"]
    TD --> T3["T3: Frontend Integration"]
    T1 --> T2
    T1 --> T3
    T2 --> T4["T4: E2E and Regression Tests"]
    T3 --> T4
    T4 --> EG["Eval Gate"]
    EG --> PP["Promotion Policy"]
```

```mermaid
graph TB
    subgraph "SENSE — What Athena Knows About Herself"
        LLM_CALLS["LLM Providers<br/>(3 call sites)"]
        LLM_CALLS -->|record_llm_latency| ATOMICS["Global Atomics<br/>latency_avg / call_count"]

        SYSINFO["sysinfo crate"]
        BOLLARD["Bollard Docker API"]
        TOOL_STORE["ToolUsageStore<br/>(SQLite)"]
        MEM_STORE["MemoryStore<br/>(SQLite + embeddings)"]
        FS["Filesystem<br/>(DB size)"]

        SYSINFO -->|RSS, CPU%| COLLECTOR["spawn_metrics_collector<br/>⏱ every 30s"]
        BOLLARD -->|container count| COLLECTOR
        TOOL_STORE -->|failure rate| COLLECTOR
        MEM_STORE -->|memory count| COLLECTOR
        FS -->|db size| COLLECTOR
        ATOMICS -->|llm latency| COLLECTOR

        COLLECTOR --> METRICS["SharedMetrics<br/>Arc&lt;RwLock&lt;SystemMetrics&gt;&gt;"]
        COLLECTOR -->|SelfMetrics event| OBSERVER["Observer Bus<br/>(UDS socket)"]
    end

    subgraph "THINK — How Athena Understands Patterns"
        METRICS -->|injected when self_dev=on| CLASSIFY["Manager::classify()<br/>task routing prompt"]

        MEM_SCANNER["spawn_memory_scanner<br/>⏱ every 1h"] -->|"last 50 memories → LLM"| PATTERNS["Memories: category='pattern'"]
        HEARTBEAT["spawn_heartbeat_loop<br/>⏱ every 30min"] -->|"HEARTBEAT.md + random memories → LLM"| HB_MEM["Memories: category='heartbeat'"]
        IDLE["spawn_idle_musings<br/>⏱ check every 5min"] -->|"random memories → LLM"| MUSE_MEM["Memories: category='musing'"]

        CODE_IDX["spawn_code_indexer<br/>⏱ every 4h"] -->|"AutonomousTask → scout ghost"| CODE_MEM["Memories: category='code_structure'"]

        CODE_MEM --> REFACTOR_SCAN["spawn_refactoring_scanner<br/>⏱ every 6h"]
        REFACTOR_SCAN -->|"code_structure → LLM analysis"| REFACTOR_MEM["Memories: category='refactoring_opportunity'"]
    end

    subgraph "ACT — How Athena Improves Herself"
        REFACTOR_SCAN -->|"30% × spontaneity gate"| AUTO_TX["auto_tx channel"]
        AUTO_TX --> AUTO_LOOP["Autonomous Task Consumer<br/>(core.rs event loop)"]
        AUTO_LOOP -->|"manager.execute_task()"| EXECUTOR["Executor → CodeStrategy"]

        EXECUTOR --> EXPLORE["Phase 1: EXPLORE<br/>read-only tools"]
        EXPLORE --> EXECUTE["Phase 2: EXECUTE<br/>CLI coding tool + ripple warning"]
        EXECUTE --> VERIFY["Phase 3: VERIFY"]

        VERIFY -->|"test_generation=true"| VERIFY_TESTS["Write #[test] functions<br/>Run test_runner<br/>Fix if failed"]
        VERIFY -->|"test_generation=false"| VERIFY_BASIC["Read diff, cargo check<br/>Report summary"]

        VERIFY_TESTS -->|success| RESULT["Pulse: AutonomousTask result"]
        VERIFY_BASIC -->|success| RESULT
    end

    subgraph "DELIVER — Bounded Communication"
        PATTERNS -->|"60% × spontaneity"| PULSE_BUS["PulseBus<br/>(broadcast)"]
        HB_MEM --> PULSE_BUS
        MUSE_MEM -->|"50% × spontaneity"| PULSE_BUS
        RESULT --> PULSE_BUS

        PULSE_BUS --> GATE["PulseGate"]
        GATE -->|"urgency + quiet hours + tolerance"| RATE["Rate Limiter<br/>4/hour max"]
        RATE --> DELIVERED["delivered_tx → Frontend"]
    end

    classDef sense fill:#1a3a5c,stroke:#4a9eff,color:#fff
    classDef think fill:#3a1a5c,stroke:#9a4aff,color:#fff
    classDef act fill:#1a5c3a,stroke:#4aff9a,color:#fff
    classDef deliver fill:#5c3a1a,stroke:#ff9a4a,color:#fff

    class LLM_CALLS,ATOMICS,SYSINFO,BOLLARD,TOOL_STORE,MEM_STORE,FS,COLLECTOR,METRICS,OBSERVER sense
    class CLASSIFY,MEM_SCANNER,HEARTBEAT,IDLE,CODE_IDX,REFACTOR_SCAN,PATTERNS,HB_MEM,MUSE_MEM,CODE_MEM,REFACTOR_MEM think
    class AUTO_TX,AUTO_LOOP,EXECUTOR,EXPLORE,EXECUTE,VERIFY,VERIFY_TESTS,VERIFY_BASIC,RESULT act
    class PULSE_BUS,GATE,RATE,DELIVERED deliver
```

## Background Process Timeline

```mermaid
gantt
    title Background Loop Scheduling (all require all_proactive=on)
    dateFormat X
    axisFormat %s

    section Always-On
    Conversation cleanup (1h)           :active, 0, 3600

    section Mood
    Mood drift (15min ±20%)             :active, 0, 900

    section Awareness
    Metrics collector (30s ±10%)        :crit, 0, 30

    section Reflection
    Heartbeat (30min ±20%)              :active, 0, 1800
    Idle check (5min ±30%)              :active, 0, 300
    Memory scanner (1h ±30%)            :active, 0, 3600

    section Self-Dev
    Code indexer (4h ±20%)              :active, 0, 14400
    Refactoring scanner (6h ±20%)       :active, 0, 21600

    section One-Shot
    Conversation re-entry (2h ±70%)     :done, 0, 7200
```

## Knob Dependency Tree

```mermaid
graph LR
    ALL["all_proactive<br/>(master switch)"] --> HB["heartbeat_enabled"]
    ALL --> CRON["cron_enabled"]
    ALL --> MOOD_EN["mood_enabled"]
    ALL --> MEM_SCAN["memory_scan_enabled"]
    ALL --> IDLE_EN["idle_musings_enabled"]
    ALL --> REENTRY["conversation_reentry_enabled"]
    ALL --> SELF_DEV["self_dev_enabled"]
    ALL --> CODE_IDX["code_indexer_enabled"]
    ALL --> REFACTOR["refactoring_scan_enabled"]

    SELF_DEV -->|"controls"| METRICS_INT["metrics_interval_secs<br/>default: 30s"]
    CODE_IDX -->|"controls"| IDX_INT["code_indexer_interval_secs<br/>default: 4h"]
    REFACTOR -->|"controls"| REF_INT["refactoring_scan_interval_secs<br/>default: 6h"]

    SPONT["spontaneity<br/>default: 0.3"] -->|"gates"| MEM_SCAN
    SPONT -->|"gates"| IDLE_EN
    SPONT -->|"gates"| REENTRY
    SPONT -->|"gates"| REFACTOR

    style ALL fill:#ff4444,color:#fff
    style SELF_DEV fill:#44aaff,color:#fff
    style CODE_IDX fill:#44aaff,color:#fff
    style REFACTOR fill:#44aaff,color:#fff
    style SPONT fill:#ffaa44,color:#000
```

---

## The Four Self-Improvement Funnels

### Funnel 1: Health Monitor → Diagnose → Auto-Fix

**Purpose**: Athena notices something is wrong with herself and fixes it.

```mermaid
flowchart TD
    A["🔍 SENSE<br/>spawn_metrics_collector (30s)"] -->|"tool_failure_rate > 0.3<br/>llm_latency > 5000ms<br/>RSS growing"| B["📊 DETECT<br/>Anomaly in SystemMetrics"]

    B -->|"metrics injected into<br/>classify prompt"| C["🧠 DIAGNOSE<br/>Manager sees health context<br/>in task routing"]

    C -->|"Pattern: tool X failing 40%"| D{"Decision Gate"}

    D -->|"User asks about it"| E1["💬 ADVISE<br/>Suggest fix to user<br/>via conversation"]

    D -->|"spontaneity gate passes<br/>+ self_dev_enabled"| E2["🔧 AUTO-FIX<br/>Dispatch AutonomousTask<br/>ghost=coder"]

    E2 --> F["CodeStrategy<br/>EXPLORE → EXECUTE → VERIFY"]
    F -->|"test_generation=true"| G["Write tests → Run → Verify"]
    G -->|"tests pass"| H["✅ Pulse: 'Fixed tool_X timeout issue'"]
    G -->|"tests fail"| I["🔄 attempt_test_fix()<br/>retry with corrected code"]

    style A fill:#1a3a5c,stroke:#4a9eff,color:#fff
    style B fill:#5c1a1a,stroke:#ff4a4a,color:#fff
    style C fill:#3a1a5c,stroke:#9a4aff,color:#fff
    style E2 fill:#1a5c3a,stroke:#4aff9a,color:#fff
    style H fill:#1a5c1a,stroke:#4aff4a,color:#fff
```

**What exists today**: Metrics collector runs, metrics injected into classify prompt. `attempt_test_fix()` is now wired into CodeStrategy self-heal when VERIFY output indicates test failures.

**What's missing**:
- No anomaly detection thresholds — metrics are collected but never compared against baselines
- No automatic dispatch when health degrades — only visible in classify prompt
- Self-heal policy is still shallow: one corrective cycle with heuristic triggering, not a deterministic multi-attempt policy
- No feedback: if a fix works, there's no record that suppresses the same alert

**How it should work**:
1. Metrics collector detects anomaly (tool failure rate spike, latency degradation, memory growth)
2. Stores anomaly as memory with category `"health_alert"`
3. If `self_dev_enabled` + spontaneity gate: dispatches diagnostic task to scout ghost
4. Scout identifies root cause → dispatches fix task to coder ghost
5. Coder runs CodeStrategy with `test_generation=true`
6. If VERIFY tests fail → `attempt_test_fix()` triggers corrective retry (currently single cycle)
7. On success: stores `"health_fix"` memory, emits pulse

---

### Funnel 2: Index → Analyze → Propose → Refactor

**Purpose**: Athena builds understanding of her own codebase and improves it structurally.

```mermaid
flowchart TD
    A["🗺️ INDEX<br/>spawn_code_indexer (4h)<br/>ghost=scout"] -->|"Extract symbols,<br/>mod/use graph,<br/>dependency map"| B["📦 STORE<br/>Memories: 'code_structure'"]

    B --> C["🔬 ANALYZE<br/>spawn_refactoring_scanner (6h)<br/>Direct LLM call"]

    C -->|"Check: >20 public symbols?<br/>Circular deps?<br/>Duplicated patterns?<br/>Large files?"| D{"Quality Gate"}

    D -->|"NO_REFACTORING"| E1["💤 Skip<br/>(check again in 6h)"]

    D -->|"Found opportunity"| E2["💡 STORE<br/>Memory: 'refactoring_opportunity'"]

    E2 --> F{"Spontaneity Gate<br/>30% × spontaneity"}

    F -->|"gate fails"| G1["📋 PROPOSE ONLY<br/>Stored as memory<br/>Available via heartbeat/conversation"]

    F -->|"gate passes"| G2["🔧 AUTO-REFACTOR<br/>AutonomousTask → coder ghost"]

    G2 --> H["CodeStrategy + test_generation=true"]
    H --> I["EXPLORE → EXECUTE (ripple warning) → VERIFY"]
    I --> J{"Tests pass?"}
    J -->|"yes"| K["✅ Pulse: 'Refactored X'<br/>Store result as memory"]
    J -->|"no"| L["❌ Store failure<br/>Suppress this idea in future"]

    style A fill:#1a3a5c,color:#fff
    style C fill:#3a1a5c,color:#fff
    style G2 fill:#1a5c3a,color:#fff
    style K fill:#1a5c1a,color:#fff
    style L fill:#5c1a1a,color:#fff
```

**What exists today**: Code indexer dispatches scout. Refactoring scanner queries memories + LLM analysis. Auto-dispatch with spontaneity gate.

**What's missing**:
- Code indexer relies on LLM compliance to store `code_structure` memories — no programmatic guarantee
- Refactoring scanner queries memories with `None` embedding (keyword-only fallback) — may miss relevant structure
- No failure feedback loop: same bad idea can be suggested every 6 hours forever
- Ripple analysis is heuristic (file extension matching), not informed by code_structure memories
- No incremental indexing — full rescan every 4h regardless of changes

**How it should work**:
1. Code indexer runs periodically, stores structural data as memories
2. Refactoring scanner queries structure + tool failure patterns + historical fixes
3. LLM identifies highest-impact opportunity
4. Stored as `refactoring_opportunity` memory (always)
5. If spontaneity allows: dispatched automatically to coder ghost
6. CodeStrategy uses code_structure memories for ripple analysis before EXECUTE
7. VERIFY phase writes tests + runs them
8. On failure: stores `"refactoring_failed"` memory with details → scanner learns to avoid similar ideas
9. On success: stores `"refactoring_done"` → enriches future analysis

---

### Funnel 3: Interact → Learn → Evolve

**Purpose**: Athena learns from conversations and proactively suggests or makes improvements.

```mermaid
flowchart TD
    A["💬 INTERACT<br/>User conversations"] -->|"save_turn()"| B["🧠 MEMORY<br/>Conversation turns<br/>+ stored memories"]

    B --> C["🔍 PATTERN SCAN<br/>spawn_memory_scanner (1h)<br/>Last 50 memories → LLM"]

    B --> D["💭 HEARTBEAT<br/>spawn_heartbeat_loop (30min)<br/>HEARTBEAT.md + random memories"]

    B --> E["🌙 IDLE MUSING<br/>spawn_idle_musings<br/>When idle > 30min"]

    C -->|"Pattern found"| F["Memory: 'pattern'"]
    D -->|"Reflection"| G["Memory: 'heartbeat'"]
    E -->|"Musing"| H["Memory: 'musing'"]

    F --> I["🔄 CROSS-POLLINATION<br/>Patterns feed into future scans,<br/>heartbeats, and refactoring analysis"]
    G --> I
    H --> I

    I -->|"'Users often ask about X<br/>but tool Y keeps failing'"| J{"Self-improvement idea?"}

    J -->|"User asks for it"| K1["🤝 COLLABORATIVE<br/>User-directed improvement"]

    J -->|"Via heartbeat pulse"| K2["💡 SUGGEST<br/>Pulse: 'I noticed pattern X,<br/>should I improve Y?'"]

    J -->|"Via refactoring scanner<br/>+ spontaneity gate"| K3["🔧 AUTONOMOUS<br/>Auto-dispatch improvement task"]

    K2 -->|"User approves"| L["CodeStrategy execution"]
    K3 --> L
    K1 --> L

    style A fill:#1a3a5c,color:#fff
    style C fill:#3a1a5c,color:#fff
    style D fill:#3a1a5c,color:#fff
    style E fill:#3a1a5c,color:#fff
    style K2 fill:#5c5c1a,color:#fff
    style K3 fill:#1a5c3a,color:#fff
```

**What exists today**: Memory scanner, heartbeat, idle musings all store reflections. Patterns accumulate. Heartbeat follows HEARTBEAT.md initiatives.

**What's missing**:
- No bridge from pattern memories to self-improvement tasks — patterns are stored but never trigger code changes
- Heartbeat can reflect on things but can't propose specific code improvements
- No mechanism to notice "users keep hitting the same bug" and auto-fix it
- HEARTBEAT.md could contain self-improvement initiatives but there's no example/template

**How it should work**:
1. Conversations and tool usage generate memories
2. Memory scanner identifies patterns ("tool X fails when Y", "users ask about Z frequently")
3. Heartbeat reflects on patterns + HEARTBEAT.md initiatives (e.g., "- Improve error messages", "- Add missing tests")
4. When a pattern has self-improvement implications:
   - Store with category `"improvement_idea"`
   - If heartbeat generates it: emit as low-urgency pulse (suggestion to user)
   - If pattern is high-confidence: feed into refactoring scanner's next analysis
5. Refactoring scanner considers improvement_idea memories alongside code_structure
6. Strong ideas get auto-dispatched; weaker ones wait for user confirmation

---

### Funnel 4: Execute → Verify → Self-Heal → Learn

**Purpose**: Every code change Athena makes feeds back into her understanding.

```mermaid
flowchart TD
    A["📋 TASK<br/>Any code modification task<br/>(user-requested or autonomous)"] --> B["🔍 EXPLORE<br/>Read-only tools<br/>Build understanding"]

    B -->|"Query code_structure memories<br/>for ripple analysis"| C["⚡ EXECUTE<br/>CLI coding tool<br/>+ RIPPLE WARNING"]

    C --> D["✅ VERIFY<br/>Read diff, run checks"]

    D -->|"test_generation=true"| E["📝 GENERATE TESTS<br/>Write #[test] functions<br/>for new/changed behavior"]

    E --> F["🧪 RUN TESTS<br/>test_runner tool"]

    F --> G{"Pass?"}

    G -->|"yes"| H["✅ RECORD SUCCESS<br/>Store: 'code_change' memory<br/>Update tool_usage stats"]

    G -->|"no"| I["🔄 SELF-HEAL<br/>attempt_test_fix()<br/>Fix implementation, re-run"]

    I --> J{"Fixed?"}
    J -->|"yes"| H
    J -->|"no, max retries"| K["❌ RECORD FAILURE<br/>Store: 'code_change_failed'<br/>Include error context"]

    H --> L["📊 FEEDBACK<br/>Enriches future:<br/>- Code indexer accuracy<br/>- Refactoring scanner judgment<br/>- Pattern scanner insights"]

    K --> L

    L -->|"'Last 3 refactorings to<br/>module X failed'"| M["🛑 SUPPRESSION<br/>Scanner learns to<br/>avoid similar changes"]

    L -->|"'All changes to module Y<br/>succeeded'"| N["🟢 CONFIDENCE<br/>Scanner can be more<br/>aggressive with Y"]

    style A fill:#1a3a5c,color:#fff
    style E fill:#3a1a5c,color:#fff
    style I fill:#5c3a1a,color:#fff
    style H fill:#1a5c1a,color:#fff
    style K fill:#5c1a1a,color:#fff
    style M fill:#5c5c1a,color:#fff
    style N fill:#1a5c3a,color:#fff
```

**What exists today**: CodeStrategy runs EXPLORE → EXECUTE → VERIFY. `test_generation` controls whether VERIFY can write tests. `attempt_test_fix()` is called in self-heal flow. Ripple warning is emitted.

**What's missing**:
- Self-heal retry policy is not deterministic across error classes and is limited to a single corrective cycle
- No success/failure recording after code changes — no feedback into future decisions
- Ripple analysis doesn't use code_structure memories, only heuristic file path matching
- No learning from outcomes: refactoring scanner can't distinguish "safe module" from "fragile module"
- No retry limit enforcement on self-heal

**How it should work**:
1. Every CodeStrategy execution records outcome as memory (`code_change` or `code_change_failed`)
2. VERIFY phase detects test failures → calls `attempt_test_fix()` using policy-driven retry limits
3. Success/failure memories feed into refactoring scanner's context
4. Scanner queries both `code_structure` and `code_change*` memories to understand what's safe to change
5. Modules with history of failed changes get lower confidence → less likely to be auto-refactored
6. Modules with clean change history → scanner can be more aggressive

---

## How The Funnels Interconnect

```mermaid
graph TB
    subgraph "Funnel 1: Health Monitor"
        F1_SENSE["Metrics (30s)"]
        F1_DETECT["Anomaly Detection"]
        F1_FIX["Auto-Fix"]
    end

    subgraph "Funnel 2: Code Evolution"
        F2_INDEX["Code Indexer (4h)"]
        F2_ANALYZE["Refactoring Scanner (6h)"]
        F2_ACT["Auto-Refactor"]
    end

    subgraph "Funnel 3: Learn from Interaction"
        F3_CONV["Conversations"]
        F3_PATTERN["Pattern Scanner (1h)"]
        F3_IDEA["Improvement Ideas"]
    end

    subgraph "Funnel 4: Execute & Learn"
        F4_CODE["CodeStrategy"]
        F4_VERIFY["Verify + Test Gen"]
        F4_HEAL["Self-Heal"]
        F4_RECORD["Record Outcome"]
    end

    %% Cross-funnel connections
    F1_SENSE -->|"tool failure rates"| F2_ANALYZE
    F1_FIX -->|"uses"| F4_CODE

    F2_ACT -->|"dispatches"| F4_CODE
    F2_INDEX -->|"code_structure memories"| F4_CODE

    F3_PATTERN -->|"improvement_idea memories"| F2_ANALYZE
    F3_CONV -->|"user requests"| F4_CODE

    F4_RECORD -->|"code_change memories"| F2_ANALYZE
    F4_RECORD -->|"enriches"| F3_PATTERN
    F4_VERIFY -->|"tool_usage stats"| F1_SENSE

    style F1_SENSE fill:#1a3a5c,color:#fff
    style F2_INDEX fill:#3a1a5c,color:#fff
    style F3_CONV fill:#5c3a1a,color:#fff
    style F4_CODE fill:#1a5c3a,color:#fff
```

**Key cross-funnel data flows:**

| From | To | Data | Purpose |
|------|----|------|---------|
| Funnel 1 (Health) | Funnel 2 (Evolution) | tool failure rates | Scanner considers failing tools as refactoring targets |
| Funnel 1 (Health) | Funnel 4 (Execute) | dispatches fix task | Health anomaly triggers CodeStrategy |
| Funnel 2 (Evolution) | Funnel 4 (Execute) | dispatches refactoring | Scanner triggers CodeStrategy |
| Funnel 2 (Evolution) | Funnel 4 (Execute) | code_structure memories | Ripple analysis before EXECUTE |
| Funnel 3 (Learning) | Funnel 2 (Evolution) | improvement_idea memories | Patterns feed scanner analysis |
| Funnel 3 (Learning) | Funnel 4 (Execute) | user requests | Direct user-driven changes |
| Funnel 4 (Execute) | Funnel 1 (Health) | tool_usage stats | Test results update tool stats |
| Funnel 4 (Execute) | Funnel 2 (Evolution) | code_change memories | Outcomes inform future refactoring |
| Funnel 4 (Execute) | Funnel 3 (Learning) | code_change memories | Patterns from change history |

---

## Bounded Autonomy Ladder

Athena's self-improvement operates at 5 levels of autonomy, each with different gates:

```
Level 5: AUTONOMOUS REFACTORING
         Gate: all_proactive + self_dev + refactoring_scan + spontaneity
         Example: "Split tools.rs into tools/*.rs modules"

Level 4: AUTONOMOUS HEALTH FIX
         Gate: all_proactive + self_dev + anomaly threshold
         Example: "Fix WebFetchTool timeout configuration"

Level 3: PROACTIVE SUGGESTION
         Gate: all_proactive + heartbeat/memory_scan + pulse tolerance
         Example: "I noticed grep tool fails 30% of the time. Want me to look into it?"

Level 2: PATTERN STORAGE
         Gate: all_proactive + memory_scan/heartbeat
         Example: Stores "pattern: tool_X failure rate increasing" as memory

Level 1: PASSIVE OBSERVATION
         Gate: all_proactive + self_dev
         Example: Metrics collected, observer events emitted, no action
```

Each level requires all the gates of levels below it, plus its own.
The user controls maximum autonomy via knob combinations.

---

## Gap Summary: What Needs Wiring

| Gap | Severity | Funnel | What's Missing |
|-----|----------|--------|----------------|
| Self-heal retry policy is shallow/non-deterministic | **HIGH** | F4 | `attempt_test_fix()` is wired, but retry/fallback behavior is not policy-driven by stable error taxonomy |
| No optimizer loop (OpenEvolve-style) | **HIGH** | Cross-funnel | No prompt/skill mutation, tournament evaluation, or merge-best promotion loop over fixed tasks |
| No anomaly detection thresholds | **HIGH** | F1 | Metrics collected but never compared against baselines |
| No outcome recording | **HIGH** | F4 | Code changes don't store success/failure memories |
| No failure suppression | **MEDIUM** | F2 | Same bad refactoring idea can recur every 6h |
| `active_tasks` always 0 | **MEDIUM** | F1 | No tracking of concurrent autonomous tasks |
| `error_rate_1h` always 0 | **MEDIUM** | F1 | Rolling error window not implemented |
| Code indexer relies on LLM | **MEDIUM** | F2 | No programmatic guarantee memories are stored |
| Ripple uses heuristics only | **LOW** | F4 | Doesn't query code_structure memories |
| Streaming latency = TTFB only | **LOW** | F1 | Doesn't measure full generation time |
| Metrics not visible to ghosts | **LOW** | F1 | Only in classify prompt, not in task execution |
