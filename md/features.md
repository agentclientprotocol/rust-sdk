# Cargo Features

The core `agent-client-protocol` crate enables `schemars` by default. Existing
dependencies that use the defaults retain JSON Schema generation and typed MCP
tool support.

## Opting out of JSON Schema generation

To use the core SDK without the `schemars` dependency:

```toml
[dependencies]
agent-client-protocol = { version = "2.2", default-features = false }
```

This disables both the SDK's direct dependency and the `schemars` feature on
`agent-client-protocol-schema`. Protocol types still support serialization and
deserialization without JSON Schema generation.

Clients, agents, proxies, sessions, and custom MCP servers continue to work.
Within `mcp_server`, the following typed tool APIs require `schemars`:

- `McpTool`
- `McpToolRegistry`, `RegisteredMcpTool`, and `EnabledTools`
- `McpToolMetadata` and `McpToolSchema`
- The `tool_fn` and `tool_fn_mut` functions

`McpServer`, `McpServerConnect`, `McpConnectionTo`, and `McpConnectionContext`
remain available without it. MCP-over-ACP attachment still only requires its
protocol feature, not JSON Schema generation.

The `agent-client-protocol-rmcp` integration enables `schemars` for its tool
builders.

Unstable protocol features are independent. For example, to use draft v2 and
MCP-over-ACP without JSON Schema generation:

```toml
[dependencies]
agent-client-protocol = { version = "2.2", default-features = false, features = ["unstable_protocol_v2", "unstable_mcp_over_acp"] }
```

To opt back in explicitly, add `features = ["schemars"]`.

## Preview subagents and initial v2 commands

Schema 1.10 adds `unstable_subagents`, forwarded by the SDK and included in
`unstable`. It exposes preview child-session and inter-session message updates
through the existing typed session notification handlers in v1 and v2. No new
transport method is needed. To combine it with v2, enable both
`unstable_subagents` and `unstable_protocol_v2`.

V2 new/resume/fork session responses now carry initial `available_commands`
alongside `config_options`. The shared schema constructors and fluent setters
are re-exported unchanged; proxies preserve these response fields. Empty command
lists are omitted on the wire, and later command changes still use session updates.
