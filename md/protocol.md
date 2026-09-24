# SDK Protocol Reference

This chapter documents the proxy extension implemented by the Rust SDK's
conductor and the opt-in native MCP-over-ACP transport exposed by the shared ACP
schema. The proxy methods are provisional SDK extensions. MCP-over-ACP is also
unstable and is available only with the `unstable_mcp_over_acp` feature.

## Method Summary

| Method | JSON-RPC shape | Purpose |
| --- | --- | --- |
| `_proxy/initialize` | request | Initialize a component as a proxy |
| `_proxy/successor` | request or notification | Forward one inner ACP message to the next component |
| `mcp/message` | agent request or provider notification | Invoke an MCP operation or carry a notification for that operation |

There are no separate request and notification method names for successor or
MCP message forwarding. The presence of an outer JSON-RPC `id` distinguishes a
request from a notification.

## Proxy Initialization

The conductor sends `_proxy/initialize` to a component that has a successor.
Its parameters are the same fields as the normal `InitializeRequest` for the
selected ACP version. Receiving this method, rather than `initialize`, tells the
component that it is running as a proxy and may forward messages with
`_proxy/successor`.

The response is the matching version's normal `InitializeResponse` result. The
stable flat `schema::InitializeProxyRequest` type uses v1; with
`unstable_protocol_v2`, `schema::v2::InitializeProxyRequest` preserves the v2
request and response types. The final agent receives the ordinary `initialize`
method and does not need to understand the proxy extension.

## Successor Forwarding

`_proxy/successor` wraps one inner ACP method and its parameters. The inner
message is flattened into the outer parameters:

```json
{
  "jsonrpc": "2.0",
  "id": 12,
  "method": "_proxy/successor",
  "params": {
    "method": "session/prompt",
    "params": {
      "sessionId": "session-1",
      "prompt": []
    }
  }
}
```

The conductor unwraps the message and sends the inner request to the next
component. The outer response carries the inner request's result or error. To
forward an inner notification, omit the outer `id`; no response is produced.
Optional extension metadata may be included as `_meta` alongside the flattened
inner message.

## Native MCP-over-ACP

Enable `unstable_mcp_over_acp` to use the draft native transport targeting MCP
2026-07-28 only. ACP initialization is unchanged; there is no MCP initialization
or connect/disconnect lifecycle. A provider adds `McpServer::Acp` to session
setup requests (`session/new`, `session/resume`, v1 `session/load`, and the
opt-in `session/fork`):

```json
{
  "type": "acp",
  "name": "project-tools",
  "serverId": "mcp-server:01"
}
```

`serverId` identifies the declared server and is used to route `mcp/message`
back to the component that provided it. A provider must not reuse one server ID
for multiple visible servers on the same ACP connection. The high-level
`agent_client_protocol::mcp_server::McpServer` APIs create this declaration
automatically.

An agent advertises `agentCapabilities.mcpCapabilities.acp: true` in v1 or
`capabilities.session.mcp.acp: {}` in draft v2. An optional
[HTTP adapter](./mcp-bridge.md) is only for agents with a modern MCP HTTP client.
Advertising HTTP support alone does not establish MCP revision compatibility.

### `mcp/message`

An agent sends one request addressed to the server, with a fresh logical MCP
request ID. This ID remains unchanged through proxies even if the outer ACP
JSON-RPC ID is renumbered:

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "method": "mcp/message",
  "params": {
    "serverId": "mcp-server:01",
    "requestId": "mcp-request:01",
    "method": "tools/call",
    "params": {
      "name": "example",
      "arguments": {},
      "_meta": {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "progressToken": "caller-supplied-token"
      }
    }
  }
}
```

The outer response carries the inner MCP result (including `resultType`) or
error directly. MRTR `input_required` is a result, not a reverse RPC; retry the
original operation with fresh metadata/IDs and unchanged opaque state.

For `server/discover`, supported versions are restricted to the revision
exposed by this binding; a backend must actually support that revision.

A provider may send notifications belonging to that operation:

```json
{
  "jsonrpc": "2.0",
  "method": "mcp/message",
  "params": {
    "serverId": "mcp-server:01",
    "requestId": "mcp-request:01",
    "method": "notifications/progress",
    "params": { "progressToken": "caller-supplied-token", "progress": 1 }
  }
}
```

Progress requires a corresponding token in the original request's inner MCP
metadata. Subscription notifications carry the listen request's logical
`requestId` in `io.modelcontextprotocol/subscriptionId`; acknowledgement comes
first. Notifications stop when their operation completes.

Both envelope types require non-null `serverId`, `requestId`, and `method`
strings. Inner `params` accepts an object or `null`; omission and `null` both
mean no parameters. A valid modern request still needs its required
`params._meta`. Optional outer ACP `_meta` is distinct from inner MCP metadata.

### Cancellation and lifetime

Use [`$/cancel_request`](./request-cancellation.md) with the outer ACP request
ID. Normal proxy forwarding maps this cancellation hop by hop. It never
rewrites the logical MCP ID.

Each operation owns its backend work. A result, error, cancellation, or
provider removal ends that operation; sibling requests and subscriptions stay
independent. There is no MCP connection ID to release. `server/discover` is an
ordinary optional request, not a prerequisite for tool calls.

## Related Documentation

- [Native MCP-over-ACP](./mcp-over-acp.md)
- [Conductor Design](./conductor.md)
- [MCP Bridge](./mcp-bridge.md)
- [Original P/ACP Design Proposal](./proxying-acp.md) (historical)
- [ACP extensibility](https://agentclientprotocol.com/protocol/extensibility)
