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
