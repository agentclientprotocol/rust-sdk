# Migrating the Native MCP Binding

The unstable connection-oriented prototype is replaced by a request-scoped
binding for **MCP 2026-07-28 only**. Standalone MCP serving is unaffected.

| Previous prototype | New binding |
| --- | --- |
| `mcp/connect`, `mcp/disconnect` | Removed |
| `McpConnectionId`, `connectionId` | Removed |
| `mcp/message(connectionId, method, params)` | `mcp/message(serverId, requestId, method, params)` |
| MCP initialization | Version and client capabilities in every request |
| Reverse MCP requests | MRTR results followed by explicit caller retries |
| Raw result or outer MCP error | Exactly one inner `result` or `error` carrier |
| Shared HTTP session and GET stream | Independent HTTP POSTs |

Keep the advertised server ID and generate a fresh `McpRequestId` per operation.
Use `MessageMcpRequest::new(server_id, request_id, method)` with MCP version and
client capabilities in the inner `_meta`. Do not send `initialize`; optional
discovery is an ordinary request.

Match `MessageMcpResponse::{Result, Error}` after handling outer ACP failure.
Never interpret an inner MCP error through ACP's error-code enum. Explicit
`data: null`, unknown error extensions, and opaque result metadata survive.
V1 and v2 carriers remain separate Rust types.

For native providers, prefer reusable `McpService` implementations and
request-scoped `McpRequestContext`. `from_rmcp` lazily creates one shared
application service for ACP execution; standalone connections still use the
factory. Scoped tool builders remain available.

`McpConnectionContext::Acp` and `McpConnectionTo::request_id()` now identify an
operation, not a connection. Standalone contexts have no logical request ID.

Observe operation cancellation and finish owned cleanup before returning.
Removing a registration or closing the ACP connection stops active work.
There is no new unadvertisement message: omitting a declaration from a later
setup request is not removal of the local registration.

The local HTTP polyfill re-exports native tools with its own server-bound
Authorization header. Preserve that header, not a bearer token in the URL.
Transport-only `x-mcp-header` annotations are normalized; an external HTTP
gateway's parameter-header policy is not transported.

The merged [connection-driver API](./migration-connection-drivers.md) is
unchanged: passive endpoints remain `None`, and owned adapters retain their
graceful-finish capability. Raw `Channel` sender/receiver types and generic
queue policies are unchanged by this migration.

See the [native guide](./mcp-over-acp.md) and
[HTTP adapter contract](./mcp-bridge.md) for usage and lifecycle details.
