# Emberloom Architecture

System-level diagrams kept in sync with the codebase via `scripts/wiring_check.py`.
Wiring violations are caught on every CI run — any variant not wired will fail the gate.

Memory retrieval hot-path design and benchmark contract:
- `docs/memory-hot-path-lru.md`

---

## Component Overview

```mermaid
graph TB
    subgraph Frontends
        TG[Telegram Bot<br/>feature = telegram]
        WH[Webhook Server<br/>feature = webhook]
        REPL[REPL / CLI]
    end

    subgraph Core["Core (core.rs)"]
        CH[CoreHandle]
        EL[Event Loop]
        ATQ[AutonomousTask Queue<br/>mpsc channel]
        PB[PulseBus<br/>broadcast channel]
    end

    subgraph Execution
        MGR[Manager]
        EXEC[Executor]
        STRAT[Strategy<br/>react / code]
        TR[ToolRegistry]
        DOCKER[DockerSession]
    end

    subgraph Background
        HB[Heartbeat<br/>15 min tick]
        SCHED[CronEngine<br/>scheduler.rs]
        PROACT[Proactive<br/>idle musing / memory scan]
        CI[CI Monitor]
        MOOD[MoodState drift]
        METRICS[SelfMetrics collector]
    end

    subgraph Persistence
        MEM[MemoryStore<br/>SQLite]
        TUS[ToolUsageStore<br/>SQLite]
        TI[TicketIntakeStore<br/>SQLite]
        KPI[KpiStore<br/>SQLite]
    end

    subgraph Observability
        OBS[ObserverHandle<br/>broadcast events]
        UDS[UDS Socket Listener]
        LFTX[Langfuse Tracer]
        DASH[HTML Dashboard]
    end

    TG -->|CoreRequest| CH
    WH -->|CoreRequest| CH
    REPL -->|CoreRequest| CH
    CH --> EL
    EL --> MGR
    EL --> ATQ
    ATQ -->|AutonomousTask| EXEC
    MGR --> EXEC
    EXEC --> STRAT
    EXEC --> TR
    TR --> DOCKER
    EXEC --> TUS
    EXEC --> OBS

    HB --> PB
    SCHED --> ATQ
    PROACT --> PB
    PROACT --> ATQ
    CI --> OBS
    MOOD --> OBS
    METRICS --> OBS

    PB -->|Pulse| TG
    PB -->|Pulse| REPL

    OBS --> UDS
    OBS --> DASH
    EXEC --> LFTX

    MEM --- MGR
    TUS --- EXEC
    TI --- EL
    KPI --- MGR
```

---

## Spark Execution State Machine

A spark is a named agent configuration. The Executor drives each task through this lifecycle:

```mermaid
stateDiagram-v2
    [*] --> Idle

    Idle --> Precheck : task dispatched
    Precheck --> Done : direct tool completion (precheck path)
    Precheck --> StrategyLoop : needs multi-step

    StrategyLoop --> ToolCall : LLM selects tool
    ToolCall --> ConfirmCheck : tool identified
    ConfirmCheck --> Denied : user denies
    ConfirmCheck --> Executing : approved / auto-approved
    Denied --> StrategyLoop : LLM retries
    Executing --> SelfHeal : tool error
    SelfHeal --> StrategyLoop : hint injected
    Executing --> StrategyLoop : tool succeeded
    StrategyLoop --> Done : max_steps reached or goal achieved

    Done --> [*]
```

---

## Pulse Delivery Pipeline

Pulses are proactive messages emitted by background tasks and delivered to frontends.

```mermaid
flowchart LR
    subgraph Sources
        HB[Heartbeat]
        MS[MemoryScan]
        IM[IdleMusing]
        AT[AutonomousTask]
        CR[ConversationReentry]
    end

    subgraph PulseBus["PulseBus (broadcast)"]
        TX{{send}}
        RX{{recv}}
    end

    subgraph Gate["PulseGate (spawn_pulse_consumer)"]
        G1{urgency?}
        G2{quiet hours?}
        G3{stochastic?}
        G4{rate limit?}
    end

    subgraph Delivery
        delivered_tx[delivered_tx channel]
        TG[Telegram]
        REPL[REPL]
    end

    Sources --> TX
    TX --> RX
    RX --> G1

    G1 -->|Silent| drop1[drop]
    G1 -->|High| G4
    G1 -->|Medium| G2
    G1 -->|Low| G2
    G2 -->|quiet hours| drop2[suppress]
    G2 -->|active| G3
    G3 -->|below tolerance| drop3[suppress]
    G3 -->|pass| G4
    G4 -->|≥ 4/hr non-High| drop4[rate-limit suppress]
    G4 -->|under limit| delivered_tx

    delivered_tx --> TG
    delivered_tx --> REPL
```

---

## CI Monitor State Machine

Triggered by `AutonomousTask` dispatch with type `CiMonitor`:

```mermaid
stateDiagram-v2
    [*] --> Polling : PR URL received

    Polling --> Passed : all checks green
    Polling --> Failed : checks failed
    Polling --> Timeout : max polls exceeded
    Polling --> Polling : pending (wait & retry)

    Failed --> ExtractLogs : fetch failure details
    ExtractLogs --> HealAttempt : logs extracted

    HealAttempt --> HealSuccess : patch applied & pushed
    HealAttempt --> HealFail : attempt limit / unrecoverable
    HealSuccess --> Polling : re-poll after push

    Passed --> [*] : emit AutonomousTask observer event
    Timeout --> [*] : emit warning
    HealFail --> [*] : emit failure pulse
```

---

## Ticket Intake Pipeline

Four provider modules feed a common dispatch in `core.rs`:

```mermaid
flowchart TD
    subgraph Providers
        GH[github.rs<br/>GitHub Issues/PRs]
        GL[gitlab.rs<br/>GitLab Issues]
        LN[linear.rs<br/>Linear Issues]
        JI[jira.rs<br/>Jira Issues]
        WH[webhook.rs<br/>Push via HTTP]
    end

    subgraph Core
        DISP[core.rs dispatch<br/>build_ticket_intake_store]
        SYNC[sync.rs<br/>TicketSyncEngine]
        STORE[TicketIntakeStore<br/>SQLite]
    end

    subgraph Output
        ATQ[AutonomousTask Queue]
        OBS[Observer<br/>TicketIntake]
    end

    GH --> DISP
    GL --> DISP
    LN --> DISP
    JI --> DISP
    WH --> DISP

    DISP --> SYNC
    SYNC --> STORE
    SYNC --> ATQ
    SYNC --> OBS
    WH --> OBS
```

---

## Observer Event Categories

All 20 categories must have at least one emit site — enforced by `scripts/wiring_check.py`:

| Category | Label | Emitted by |
|---|---|---|
| `Startup` | `STARTUP` | core.rs (init) |
| `KnobChange` | `KNOB` | main.rs, telegram.rs |
| `Heartbeat` | `HEARTBEAT` | heartbeat.rs, proactive.rs |
| `CronTick` | `CRON` | scheduler.rs |
| `MoodChange` | `MOOD` | mood.rs (drift) |
| `MemoryScan` | `MEMORY` | proactive.rs |
| `StochasticRoll` | `STOCHASTIC` | heartbeat.rs, proactive.rs |
| `PulseEmitted` | `PULSE+` | pulse.rs (consumer) |
| `PulseSuppressed` | `PULSE_X` | pulse.rs (consumer) |
| `PulseDelivered` | `PULSE_OK` | pulse.rs (consumer) |
| `IdleMusing` | `IDLE` | proactive.rs |
| `EnergyShift` | `ENERGY` | mood.rs (drift, delta ≥ 0.1) |
| `ChatIn` | `CHAT_IN` | core.rs (request) |
| `ChatOut` | `CHAT_OUT` | core.rs (response) |
| `AutonomousTask` | `AUTO_TASK` | core.rs, proactive.rs |
| `TicketIntake` | `TICKET` | core.rs, ticket_intake/ |
| `ToolUsage` | `TOOL_USE` | executor.rs (every tool call) |
| `ToolReload` | `TOOL_RELOAD` | dynamic_tools.rs |
| `SelfMetrics` | `SELF_METRICS` | introspect.rs |
| `CiMonitor` | `CI_MON` | ci_monitor.rs |

---

## Data Flow: Chat Request → Response

```mermaid
sequenceDiagram
    participant FE as Frontend (Telegram/REPL)
    participant CH as CoreHandle
    participant EL as Event Loop
    participant MEM as MemoryStore
    participant MGR as Manager
    participant LLM as LLM Provider
    participant EXEC as Executor
    participant TOOL as Tool

    FE->>CH: CoreRequest::Chat { message }
    CH->>EL: mpsc send
    EL->>MEM: load_recent_conversation()
    EL->>MGR: chat(messages, session)
    MGR->>LLM: complete(system_prompt + history)
    LLM-->>MGR: response (or tool_call)
    alt Tool call
        MGR->>EXEC: execute_tool(name, params)
        EXEC->>TOOL: tool.execute(docker, params)
        TOOL-->>EXEC: ToolResult
        EXEC->>MGR: tool output
        MGR->>LLM: complete(... + tool_result)
        LLM-->>MGR: final response
    end
    MGR-->>EL: String response
    EL->>MEM: store_conversation(user, assistant)
    EL-->>FE: CoreEvent::Response
```

---

## Wiring Invariants (CI-enforced)

`scripts/wiring_check.py` validates on every PR:

1. **ObserverCategory** — every variant has ≥ 1 `observer.log(ObserverCategory::X, …)` call
2. **PulseSource** — every non-`#[cfg(test)]` variant appears in `Pulse::new(PulseSource::X, …)`
3. **LlmProvider** — every `impl LlmProvider for T` is referenced in `config.rs build_llm_provider_for`
4. **TicketIntake** — every provider module in `src/ticket_intake/` is referenced in `core.rs`
5. **PulseBus** — subscription path exists (consumer is active)
6. **Executor** — holds `ObserverHandle` and emits `ToolUsage`
7. **MoodState::drift** — emits `EnergyShift`
