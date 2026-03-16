# Emberloom Examples

Runnable examples demonstrating common Emberloom workflows.

| File | Description |
|---|---|
| [basic-dispatch.sh](basic-dispatch.sh) | Copy config, run a health check, and dispatch a task from the command line |
| [feature-contract.toml](feature-contract.toml) | Fan-out/fan-in feature contract template (TOML) |
| [feature-contract-linear.toml](feature-contract-linear.toml) | Linear-chain feature contract template (TOML) |
| [custom-ghost.toml](custom-ghost.toml) | Minimal spark configuration snippet showing personality and tool customization |

---

## Prerequisites

All examples assume:

1. You have a working `config.toml` (copy from `config.example.toml`).
2. Docker daemon is running.
3. Your LLM provider credentials are configured (or use `provider = "ollama"` for local inference).

---

## Running the Basic Dispatch Example

```bash
chmod +x examples/basic-dispatch.sh
./examples/basic-dispatch.sh
```

This will:
1. Copy `config.example.toml` → `config.toml` (if not already present)
2. Run `doctor --skip-llm` to verify the environment
3. List available spark agents
4. Dispatch a simple "hello world" task to the `coder` spark

---

## Using a Feature Contract

Feature contracts can be YAML, JSON, or TOML.
See [docs/feature-contract-workflow.md](../docs/feature-contract-workflow.md) for the full workflow.

```bash
cp examples/feature-contract.toml my-feature.toml
# Edit my-feature.toml to describe your feature
athena feature validate --file my-feature.toml
athena feature plan --file my-feature.toml
```

---

## Customising a Spark

Place your custom spark config fragment into `config.toml` under a `[[ghosts]]` section
(or in `~/.athena/ghosts/<name>.toml` for user-local overrides):

```bash
# Append the custom spark snippet to your config
cat examples/custom-ghost.toml >> config.toml
```
