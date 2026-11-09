# SDK Protocol Reference

This chapter documents shared schema features, the proxy chain methods
implemented by the Rust SDK's conductor, and the opt-in native MCP-over-ACP
transport. The proxy methods come from the
[ACP proxy chains RFD](https://agentclientprotocol.com/rfds/proxy-chains) and
are unstable in the schema, behind its `unstable_proxy_chains` feature. The SDK
always enables that feature, so proxy and conductor support needs no feature
flag. MCP-over-ACP is also unstable and is available only with the
`unstable_mcp_over_acp` feature.

## Client-provided system prompts

Schema 1.16 stabilizes client-authored instructions, so they need no feature
flag. In v1, check `agentCapabilities.sessionCapabilities.systemPrompt`; in v2,
check `capabilities.session.systemPrompt`. Omitted or `null` means unsupported.
An object advertises support for text content in `append` mode. Only request
`override` if the agent also advertises `systemPrompt.mode.override`.

Use the version's `SystemPrompt::new` builder and pass the complete request
through the SDK's `_from` session helpers:

```rust
use agent_client_protocol::v1::schema;

let instructions = schema::SystemPrompt::new(vec![
    "Review changes without modifying files.".into(),
]);
let request = schema::NewSessionRequest::new("/workspace/project")
    .system_prompt(instructions);
// After checking the agent's advertised capability:
// connection.build_session_from(request).block_task().start_session().await?;
# let _ = request;
```

In v2, this needs `unstable_protocol_v2` and uses the equivalent `v2::schema`
types.
`SystemPrompt::new` defaults to `append` in Rust, but both `mode` and `content`
are required on the wire. Omitted or `null` `systemPrompt` supplies no client
instructions. Malformed values fail typed request decoding with `-32602`;
unknown modes are preserved for forwarding, not treated as append.

Clients should re-send the prompt on load (v1), resume, and fork (with
`unstable_session_fork`). Agents that advertise support must apply a supplied
prompt or reject the lifecycle operation, not silently create or restore a
session without it. Override replaces agent-authored default instructions, not
tools, permissions, or workspace instructions. Only supply content trusted at
system-instruction precedence.

## Method Summary

| Method | JSON-RPC shape | Purpose |
| --- | --- | --- |
| `proxy/initialize` | request | Initialize a component as a proxy |
| `proxy/successor` | request or notification | Forward one inner ACP message to the next component |
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

> Earlier SDK releases used `_proxy/initialize` and `_proxy/successor`. See
> [Migrating to v5](./migration-v5.md#proxy-method-names); conductors and
> proxies must be upgraded together.

The conductor sends `proxy/initialize` to a component that has a successor.
Its parameters are the same fields as the normal `InitializeRequest` for the
selected ACP version. Receiving this method, rather than `initialize`, tells the
component that it is running as a proxy and may forward messages with
`proxy/successor`.

The response is the matching version's normal `InitializeResponse` result. The
SDK uses the schema crate's types, one per version:
`v1::schema::InitializeProxyRequest` and, with `unstable_protocol_v2`,
`v2::schema::InitializeProxyRequest`. Each is answered with that version's
`InitializeProxyResponse`, a transparent wrapper around its
`InitializeResponse`. Convert a forwarded `initialize` response with `.into()`.
The final agent receives the ordinary `initialize` method and does not need to
understand the proxy extension.

## Successor Forwarding

`proxy/successor` wraps one inner ACP method and its parameters. The outer
parameters hold the inner message's `method` and `params`:

```json
{
  "jsonrpc": "2.0",
  "id": 12,
  "method": "proxy/successor",
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
component. The outer response carries the inner request's result or error
unchanged. To forward an inner notification, omit the outer `id`; no response
is produced. The inner `params` may be omitted or `null` when the inner message
has none. Optional metadata for the envelope itself may be included as `_meta`
next to `method` and `params`; the inner message keeps its own `_meta` inside
its `params`.

`$/cancel_request` is hop-local and is never wrapped. To cancel a
`proxy/successor` request, a proxy sends a plain `$/cancel_request` whose
`requestId` is the ID of that `proxy/successor` request. The conductor then
cancels the request it forwarded to the next component, with that hop's ID.
The conductor still accepts a `$/cancel_request` wrapped in `proxy/successor`
and treats it the same way.

Only a proxy and its conductor send `proxy/successor`. The conductor answers a
`proxy/successor` request from the agent, or from the client of a conductor in
agent mode, with `-32600` and drops such a notification, without forwarding
either. The same applies to a `proxy/successor` wrapped in another
`proxy/successor`.

The schema types for the envelope are `ProxySuccessorRequest`,
`ProxySuccessorNotification` and `ProxySuccessorResponse` in `v1::schema` and
`v2::schema`, with the `ProxyRequest`, `ProxyResponse` and `ProxyNotification`
unions and the method names in `PROXY_METHOD_NAMES`. They hold the inner
message as raw JSON. `schema::SuccessorMessage<M>` is the SDK's typed view of
the same envelope: it holds the inner message as an SDK message type, works for
every protocol version, and for a wrapped request uses the inner request's
response type. The SDK's proxy routing wraps and unwraps messages with it.

### Results and errors

The result or error of a wrapped request reaches the original requester
unchanged, in both directions: error codes, messages and `data` (including an
explicit `null`) are not rewritten, and results are not re-encoded through
typed schema structs by the conductor or by the default proxy forwarding.
Two limits remain. Every hop parses messages into `serde_json::Value`, so a
number is re-encoded in serde_json's form (`1E2` becomes `100.0`) and an
integer outside the 64-bit range loses precision. Typed ACP dispatch also drops
members of an error object other than `code`, `message` and `data`, which
JSON-RPC does not define.

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
- [ACP extensibility](https://agentclientprotocol.com/protocol/v1/extensibility)
