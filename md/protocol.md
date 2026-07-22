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
| `mcp/connect` | request | Open a connection to an ACP-provided MCP server |
| `mcp/message` | request or notification | Carry one inner MCP message over ACP |
| `mcp/disconnect` | request | Close an MCP-over-ACP connection |

There are no separate request and notification method names for successor or
MCP message forwarding. The presence of an outer JSON-RPC `id` distinguishes a
request from a notification.

## Proxy Initialization

The conductor sends `_proxy/initialize` to a component that has a successor.
Its parameters are the same fields as a normal v1 `InitializeRequest`. Receiving
this method, rather than `initialize`, tells the component that it is running as
a proxy and may forward messages with `_proxy/successor`.

The response is a normal `InitializeResponse` result. The final agent receives
the ordinary `initialize` method and does not need to understand the proxy
extension.

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

`serverId` identifies the declared server and is used to route `mcp/connect`
back to the component that provided it. A provider must not reuse one server ID
for multiple visible servers on the same ACP connection. The high-level
`agent_client_protocol::mcp_server::McpServer` APIs create this declaration
automatically.

An agent that consumes this transport advertises
`agentCapabilities.mcpCapabilities.acp`. If the final agent supports HTTP but
not ACP-transport MCP servers, place the [MCP-over-ACP compatibility
bridge](./mcp-bridge.md) immediately before it.

### `mcp/connect`

The MCP client opens a connection to the declared server ID:

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "method": "mcp/connect",
  "params": { "serverId": "mcp-server:01" }
}
```

The provider creates one active MCP connection and returns a distinct
connection ID:

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "result": { "connectionId": "mcp-connection:01" }
}
```

The server ID selects what to connect to; the connection ID selects that
particular running connection. All subsequent messages use the connection ID.

### `mcp/message`

`mcp/message` carries one inner MCP method and its named parameters. The method
is bidirectional because MCP clients and servers can both issue requests:

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "method": "mcp/message",
  "params": {
    "connectionId": "mcp-connection:01",
    "method": "tools/call",
    "params": {
      "name": "example",
      "arguments": {}
    }
  }
}
```

Use an outer request for an inner MCP request and an outer notification for an
inner MCP notification. The outer response carries the inner MCP result or
error.

### `mcp/disconnect`

Disconnect is a request so the caller knows that the provider has released the
active connection:

```json
{
  "jsonrpc": "2.0",
  "id": 22,
  "method": "mcp/disconnect",
  "params": { "connectionId": "mcp-connection:01" }
}
```

A successful disconnect returns an empty result:

```json
{
  "jsonrpc": "2.0",
  "id": 22,
  "result": {}
}
```

## Related Documentation

- [Conductor Design](./conductor.md)
- [MCP Bridge](./mcp-bridge.md)
- [Original P/ACP Design Proposal](./proxying-acp.md) (historical)
- [ACP extensibility](https://agentclientprotocol.com/protocol/extensibility)
