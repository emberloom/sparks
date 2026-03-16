# MCP Integration

Emberloom can discover and expose Model Context Protocol (MCP) tools through a config-driven registry.

## Status

Implemented with namespaced tool registration and tool-level allowlists.

Current transport support:
- `stdio` (supported)
- `sse` / `websocket` (config enum exists, currently rejected at runtime)

## Configuration

```toml
[mcp]
enabled = true
discovery_ttl_secs = 60

[[mcp.servers]]
name = "linear"
enabled = true
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-linear"]
env = ["LINEAR_API_KEY"]
timeout_secs = 30
reconnect_delay_secs = 5
requires_confirmation = true
allowed_tools = ["search_documents", "get_issue", "mcp:linear:search_documents"]
```

## Tool Naming & Allowlist Rules

Discovered tools are exposed as:
- `mcp:<server>:<remote_tool>`

Allowlist behavior:
- `allowed_tools` is required for exposure.
- Empty `allowed_tools` means no MCP tools are exposed.
- Allowed entries may be:
  - `*`
  - raw remote tool name (e.g. `search_documents`)
  - namespaced tool name (e.g. `mcp:linear:search_documents`)

## Confirmation Behavior

`requires_confirmation` is propagated into the tool wrapper and participates in normal tool confirmation flow.

## Failure Modes

Common setup errors:
- missing `command` for `stdio` server
- unsupported transport (`sse`/`websocket`)
- missing auth env var listed in `env`
- `timeout_secs` too low for server startup or tool response

## Recommended Validation

- start with one server and one explicit tool in `allowed_tools`
- run `athena ghosts` and a controlled task that calls the MCP tool
- monitor observer logs for MCP discovery and execution errors
