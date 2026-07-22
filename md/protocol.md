# Proxy Extension Protocol Reference

This chapter documents the extension methods implemented by the Rust SDK's
conductor and MCP-over-ACP polyfill. These methods are provisional SDK
extensions. They are separate from stable ACP methods and from the draft native
MCP-over-ACP methods described below.

## Method Summary

| Method | JSON-RPC shape | Purpose |
| --- | --- | --- |
| `_proxy/initialize` | request | Initialize a component as a proxy |
| `_proxy/successor` | request or notification | Forward one inner ACP message to the next component |
| `_mcp/connect` | request | Open a legacy polyfill MCP connection |
| `_mcp/message` | request or notification | Carry one inner MCP message through the legacy polyfill |
| `_mcp/disconnect` | notification | Close a legacy polyfill MCP connection |

There are no `_proxy/successor/request`, `_proxy/successor/notification`,
`_mcp/request`, or `_mcp/notification` methods. The presence of an outer
JSON-RPC `id` distinguishes requests from notifications.

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

## Legacy MCP Polyfill Methods

The underscore-prefixed `_mcp/*` family is used by
`agent-client-protocol-polyfill` for its legacy `acp:` URL compatibility path.
It is not the draft native ACP MCP transport.

### `_mcp/connect`

The polyfill opens a connection to the component identified by `acp_id`:

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "method": "_mcp/connect",
  "params": { "acp_id": "acp:server-1" }
}
```

The result contains a polyfill connection identifier:

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "result": { "connection_id": "connection-1" }
}
```

### `_mcp/message`

`_mcp/message` flattens one inner MCP method into its parameters. This method is
bidirectional because MCP clients and servers can both issue requests:

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "method": "_mcp/message",
  "params": {
    "connectionId": "connection-1",
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

### `_mcp/disconnect`

The disconnect notification ends the named polyfill connection:

```json
{
  "jsonrpc": "2.0",
  "method": "_mcp/disconnect",
  "params": { "connection_id": "connection-1" }
}
```

The local extension types intentionally retain their existing serialized field
names: `acp_id` and `connection_id` for connect/disconnect, but `connectionId`
inside `_mcp/message`.

## Draft Native MCP-over-ACP

With the `unstable_mcp_over_acp` feature, the protocol schema also exposes the
draft native transport. A server is declared as `McpServer::Acp`, serialized
with `type: "acp"`, `name`, and `serverId`. Native messages use
`mcp/connect`, `mcp/message`, and `mcp/disconnect` without a leading underscore
and use the schema's camel-case fields.

The native and legacy method families are not interchangeable. Prefer the
native schema types for new implementations that explicitly opt into the draft
feature. Use the [MCP Bridge](./mcp-bridge.md) only to adapt the legacy
`McpServer::Http` `acp:` declaration for an agent that cannot consume that
routed form directly.

## Related Documentation

- [Conductor Design](./conductor.md)
- [MCP Bridge](./mcp-bridge.md)
- [Original P/ACP Design Proposal](./proxying-acp.md) (historical)
- [ACP extensibility](https://agentclientprotocol.com/protocol/extensibility)
