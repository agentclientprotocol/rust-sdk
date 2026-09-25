# Migrating the Native MCP Transport

This draft replaces the connection-oriented MCP-over-ACP prototype with a
request-scoped binding for **MCP 2026-07-28 only**. It is part of the next major
SDK change, not a compatibility layer for older MCP revisions. The
`unstable_mcp_over_acp` gate remains; draft ACP v2 still has its separate gate.

## Wire changes

| Previous prototype | New binding |
| --- | --- |
| `mcp/connect` and `mcp/disconnect` | Removed |
| `McpConnectionId` / `connectionId` | Removed |
| `mcp/message(connectionId, method, params)` | `mcp/message(serverId, requestId, method, params)` |
| MCP initialization and connection-scoped capabilities | Required version/capabilities in each request's inner `_meta` |
| Arbitrary reverse MCP requests | MRTR `input_required` results and explicit caller retries |
| Raw MCP result or MCP error in the ACP error envelope | Successful ACP response containing exactly one inner `result` or `error` |
| HTTP MCP sessions and standalone GET streams | Independent POSTs, including long-lived subscription POSTs |

Keep the server declaration's `serverId`. Generate a fresh logical
`McpRequestId` per call and pass it to
`MessageMcpRequest::new(server_id, request_id, method)`. That ID stays unchanged
through proxies; it is not the hop-local ACP JSON-RPC ID.

Every inner request includes:

```json
{
  "_meta": {
    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
    "io.modelcontextprotocol/clientCapabilities": {}
  }
}
```

There is no hidden initialization or discovery prerequisite. Explicitly select
2026-07-28 when constructing an rmcp client: rmcp 3.4's default version constant
still selects an older revision.

## Handle two error domains

First handle the outer ACP request result, then match `MessageMcpResponse`:

- `Result { result, .. }` contains an opaque MCP result, including any MCP
  metadata, `resultType`, or explicit JSON null.
- `Error { error, .. }` contains an `McpError`. Its `code` is a plain MCP integer,
  not ACP's `ErrorCode`. `data` preserves omission separately from JSON null,
  and unknown error extensions survive.
- An outer ACP error reports a binding failure: invalid envelope, cancellation,
  resource limit, unavailable registration, or backend/transport failure.

Do not run ACP authentication handling on an inner MCP error code. A tool
execution failure with `isError` remains an MCP result. MRTR's `input_required`
also remains a result; retry with fresh IDs/metadata and unchanged opaque state.

ACP v1 and v2 define independent response/error carrier types. They currently
use the same JSON representation, but may evolve separately. Use the types
for the negotiated ACP version and keep trait implementations version-specific.

## Separate services from operations

Use the reusable `McpService` abstraction for native providers. Per-operation
`McpRequestContext` contains logical/server identity, MCP metadata/capabilities,
cancellation, and request-scoped notification permissions. A service can share
application state without sharing MCP protocol state.

`McpServer::new_service` registers a native service. An explicit factory/standalone
adapter remains available when constructing a backend per operation is actually
needed. `McpServer::from_rmcp` and the rmcp tool builder retain their attachment
entry points but execute ACP requests through the request-native service path.

Do not detach tool work from its operation. Cancelling a queued call must prevent
it from starting; cancelling a running call must drop or stop its owned future
and supervise cleanup. Failure to deliver a cancelled tool's result must not
terminate the containing ACP connection.

## Registration and cancellation

A server ID names one registration during an ACP connection's lifetime. Do not
rebind a removed ID to another provider. Dropping the local registration rejects
future calls and cancels its active work; omitting a declaration from a later
setup request is not a new unadvertisement message.

Use ACP request cancellation, not an MCP disconnect. Cancellation revokes
notifications immediately while cleanup retains the active ID and admission
permit. Independent calls, subscriptions, and the reusable service remain alive.
Transport EOF must begin this cleanup even if application code is still waiting
on the disconnected peer.

## HTTP clients

The local polyfill re-exports native tools through one signed, loopback HTTP
endpoint per ACP connection. Pass the declaration's Authorization header, never
put its bearer credential in a URL. Requests use current MCP headers and do not
exchange session IDs or `initialize`.

The endpoint strips transport-only `x-mcp-header` schema annotations from tool
descriptors and rejects `Mcp-Param-*` headers. It does not transport an existing
HTTP gateway's routing or authorization policy. Direct tool calls require no
preliminary descriptor fetch. See the [HTTP adapter contract](./mcp-bridge.md).

## Custom transports and connectors

`ConnectTo::into_channel_and_future` now returns `(Channel, ConnectionDriver)`.
Wrap an owned driver future in `ConnectionDriver::new`; use
`ConnectionDriver::passive()` only for an endpoint driven elsewhere. Awaiting
the driver remains supported. Do not treat passive-driver completion as EOF:
doing so drops final responses when an input stream half-closes.

`Channel::rx` yields `BudgetedFrame`, not a bare wire frame. Use `.frame()` to
inspect it, and preserve the envelope when forwarding through a sink. If a
custom adapter separates payload from accounting with `.into_parts()`, retain
the permit as long as the deferred payload or serialized output exists.

For a new raw frame, use `FrameSender::send_frame(frame).await` outside dispatch
or `try_send(frame)` for explicit fail-fast admission. The old `unbounded_send`
API is removed; ignoring capacity errors silently loses protocol traffic.
Finite queue and byte policies are configured through `ConnectionLimits`.

## Release checklist

- Replace the draft Git schema pin with the released matching schema version.
- Coordinate major releases for crates whose public transport or rmcp-facing
  API changed; do not infer compatibility solely from unchanged Cargo numbers.
- Exercise v1 and v2 carrier/error behavior, cancellation and EOF, MRTR,
  subscriptions, and slow consumers before stabilizing.
- Follow the bounded transport API's ownership rules when writing custom
  adapters: moving a payload must not release its accounting while a deferred
  dispatch, writer, or unread HTTP body still retains it.

The [native guide](./mcp-over-acp.md) and [protocol reference](./protocol.md#native-mcp-over-acp)
describe the target behavior. Historical migration chapters describe earlier
releases and are not a specification for this binding.
