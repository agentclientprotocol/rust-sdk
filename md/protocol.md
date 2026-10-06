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
| `mcp/message` | request or notification | Invoke one MCP operation or carry its notification |

There are no separate request and notification method names for successor or
MCP message forwarding. The presence of an outer JSON-RPC `id` distinguishes a
request from a notification.

## Raw JSON-RPC Responses

Transport frames carry protocol-neutral `RawJsonRpcResponse` values. Their
boxed `RawJsonRpcError` preserves numeric error codes, distinguishes omitted
`data` from explicit null, and retains error extension fields. Raw relays
should forward these values without ACP interpretation.

Typed ACP dispatch converts raw errors to `Error` explicitly using
`RawJsonRpcError::into_acp_error`; ACP error codes are interpreted at that
boundary and error extension fields are discarded. `RawJsonRpcMessage::response`
still accepts an ACP `Result<Value, Error>` and converts its error to the raw
representation. Other protocols should construct `RawJsonRpcResponse` directly.

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

Enable `unstable_mcp_over_acp` to use the draft native transport. A component
providing an MCP server adds `McpServer::Acp` to session setup requests
(`session/new`, `session/load`, `session/resume`, and the opt-in `session/fork`).
Its wire shape contains a human-readable name and an opaque server identifier:

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

An agent that consumes this transport advertises
`agentCapabilities.mcpCapabilities.acp`. If the final agent supports HTTP but
not ACP-transport MCP servers, place the [MCP-over-ACP compatibility
bridge](./mcp-bridge.md) immediately before it.

### `mcp/message`

An agent-to-provider request invokes one MCP operation. A provider-to-agent
notification belongs to that active operation. This is an ACP envelope choice,
not an MCP method-name convention: the outer `id` distinguishes message kind,
and inner methods such as `tools/call` and `notifications/progress` stay distinct.
Reverse requests are not part of this binding.

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "method": "mcp/message",
  "params": {
    "serverId": "mcp-server:01",
    "requestId": "logical-request:01",
    "method": "tools/call",
    "params": {
      "name": "example",
      "arguments": {},
      "_meta": {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
      }
    }
  }
}
```

The logical ID stays stable through proxies; the outer ACP ID is hop-local.
Each inner request declares its version and capabilities. This binding exposes
MCP 2026-07-28 only, without `initialize` or a discovery prerequisite.

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "result": {
    "result": { "content": [] }
  }
}
```

Successful ACP responses carry exactly one inner `result` or `error`.
An MCP error's code, omitted/null data, and extensions retain MCP meaning.
Outer ACP errors describe binding failures. MRTR's `input_required` remains an
inner result; subscriptions remain active requests with scoped notifications.

Cancellation uses ACP's existing best-effort mechanism. The Rust SDK supervises
owned cleanup, but that stronger implementation behavior is not an extra
requirement for advertising the transport capability.

See the [native guide](./mcp-over-acp.md) and
[migration guide](./migration-stateless-mcp.md). The previous connection IDs
and connect/disconnect methods are removed.

## Related Documentation

- [Conductor Design](./conductor.md)
- [MCP Bridge](./mcp-bridge.md)
- [Original P/ACP Design Proposal](./proxying-acp.md) (historical)
- [ACP extensibility](https://agentclientprotocol.com/protocol/extensibility)
