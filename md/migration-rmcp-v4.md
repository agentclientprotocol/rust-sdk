# Migrating the rmcp Integration to v4

The next major release of `agent-client-protocol-rmcp` upgrades its public
`rmcp` dependency from 2.x to 3.4. This is a breaking change for integrations
that pass rmcp services or types across the crate boundary. It ships together
with the core `agent-client-protocol` 3.x release, which also changes public
APIs. Migrate both public dependencies together; the minimum supported Rust
version remains 1.88.

| Integration crate | Core ACP SDK | MCP SDK |
| --- | --- | --- |
| `agent-client-protocol-rmcp` 4.x (unreleased) | 3.x | `rmcp` 3.x |
| `agent-client-protocol-rmcp` 3.x | 2.x | `rmcp` 2.x |

Upgrade the application's core ACP and rmcp dependencies together with the
integration crate. See the [core 3.x migration guide](./migration-v3.md) for
raw-response API changes and links to the transport migrations.
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

The dependency requirement supports rmcp 3.4 and later compatible 3.x versions;
the workspace lockfile currently resolves 3.5. Their version constants differ:

- In rmcp 3.4, `ProtocolVersion::LATEST` is 2025-11-25.
- In rmcp 3.5, `ProtocolVersion::LATEST` is 2026-07-28, which has no initialize
  handshake. `LATEST_WITH_INITIALIZE` is 2025-11-25.

Do not treat a changing `LATEST` constant or dependency upgrade alone as
protocol selection. For modern no-initialize requests, explicitly select
`ProtocolVersion::V_2026_07_28` and include the required request metadata.
For a legacy initialize handshake, select its supported revision; with rmcp
3.5, `LATEST_WITH_INITIALIZE` names the latest such revision. The native ACP
binding below requires modern per-request metadata regardless of the resolved
rmcp version.

## Native attachments also migrate in this release

The combined release replaces ACP's unstable `mcp/connect` / `mcp/disconnect`
binding with request-scoped, server-addressed `mcp/message` operations for
**MCP 2026-07-28 only**, without initialization. Migrate native attachments
using the [native binding migration](./migration-stateless-mcp.md) and
[request-scoped MCP guide](./mcp-over-acp.md). `from_rmcp` lazily creates one
shared application service for native requests, whose metadata, notifications,
and cancellation belong to each operation.

Standalone rmcp connections still use the factory per connection and retain
rmcp's protocol negotiation; the native binding's modern-only requirement does
not apply to them. Custom transports and low-level connection callers must
also follow the [connection-driver migration](./migration-connection-drivers.md)
for the core 3.x API.
