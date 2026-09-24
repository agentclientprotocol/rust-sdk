# Migrating the rmcp Integration to v4

The next major release of `agent-client-protocol-rmcp` upgrades its public
`rmcp` dependency from 2.x to 3.4. This is a breaking change for integrations
that pass rmcp services or types across the crate boundary. The core
`agent-client-protocol` SDK remains on 2.x, and the minimum supported Rust
version remains 1.88.

| Integration crate | Core ACP SDK | MCP SDK |
| --- | --- | --- |
| `agent-client-protocol-rmcp` 4.x (unreleased) | 2.x | `rmcp` 3.x |
| `agent-client-protocol-rmcp` 3.x | 2.x | `rmcp` 2.x |

Upgrade the application's rmcp dependency together with the integration crate.
A service implementing rmcp 2.x's `Service` cannot be passed to the new
`McpServer::from_rmcp`, even when it provides the same tools.

## Custom services and tools

- Use `ServerConfig` and `ClientConfig` instead of the deprecated `ServerInfo`
  and `ClientInfo` aliases.
- A manual `ServerHandler::call_tool` implementation now returns
  `Result<CallToolResponse, ErrorData>`. Convert a completed `CallToolResult`
  with `.into()`. The response enum also represents MRTR input-required
  results and task-extension results; do not assume every response is a
  completed tool result.
- Functions registered with rmcp's `#[tool]` macro can still return
  `CallToolResult`. The ACP integration's `tool` and `tool_fn` builder APIs also
  remain available, and their results become completed tool responses.
- `ToolExecution` and `Tool::with_execution` are no longer part of the tool
  model. Do not add the old task-execution marker to tool definitions.

For example, a manual handler that previously returned
`Ok(CallToolResult::structured(value))` returns
`Ok(CallToolResult::structured(value).into())` under its new
`CallToolResponse` return type.

## Modern MCP is now available to supplied services

rmcp 3.4 implements discovery, per-request metadata, modern result shapes,
MRTR, and subscription APIs for MCP 2026-07-28. The adapter preserves those
requests and results when using `McpServer::from_rmcp`; a supplied service is
still responsible for its advertised capabilities and handlers.

The integration tests exercise both the built-in tool server and a supplied
rmcp service with actual 2026-07-28 requests, without sending `initialize`.
They cover direct tool calls before discovery, discovery itself, per-request
version errors, and an MRTR retry with fresh request metadata.

Do not treat the version-string constant or dependency upgrade alone as
protocol selection. rmcp 3.4's `ProtocolVersion::LATEST` still defaults to
2025-11-25. A modern client must select 2026-07-28 explicitly and include
the required request metadata.

## MCP-over-ACP remains a separate draft

This upgrade does not change ACP's unstable `mcp/connect`, `mcp/message`, or
`mcp/disconnect` envelopes. Redesigning that transport around stateless,
server-addressed requests is separate work. The new transport's latest-only
target does not require removing existing rmcp behavior from this prerequisite
dependency upgrade.
