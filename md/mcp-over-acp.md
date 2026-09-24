# Native MCP-over-ACP

The native transport targets MCP 2026-07-28 only. It lets an ACP client or proxy
provide MCP tools to an agent over the existing ACP connection, without a
conductor, HTTP listener, subprocess, or MCP initialization handshake.

Enable `unstable_mcp_over_acp` on the core SDK. Draft ACP v2 additionally
requires `unstable_protocol_v2`. The shared-schema revision is currently pinned
to a Git commit for cross-repository validation; replace that pin with the
released schema before publishing the SDK.

## Providing tools

Attach an `mcp_server::McpServer` to session setup through the existing builder
APIs. It publishes a `McpServer::Acp` declaration with a provider-generated
`serverId`.

Each incoming `mcp/message` invokes the backend factory for one operation.
The MCP request context exposes `server_id()` and `request_id()`; standalone
MCP serving has neither. Tool definitions can be shared, but per-request MCP
metadata and capabilities must not be inferred from previous operations.

The rmcp integration can construct tools through its builder or wrap a supplied
rmcp 3.4 service. The normal rmcp service can process a modern request without
`initialize` when its inner `_meta` declares the modern version and capabilities.

## Consuming tools

An ACP agent holds a `ConnectionTo<Client>` or its v2 counterpart. It sends
`MessageMcpRequest::new(server_id, request_id, method)` with the inner MCP
parameters, including:

- `io.modelcontextprotocol/protocolVersion: "2026-07-28"`;
- `io.modelcontextprotocol/clientCapabilities` as an object;
- any request-specific identity, progress token, extension settings, or retry
  state required by the MCP operation.

Choose a fresh logical request ID. It becomes the MCP JSON-RPC ID and remains
unchanged through proxies. The outer ACP request ID is separate and may change
on each hop.

Register a `MessageMcpNotification` handler before sending requests that may
stream notifications. Route by server and logical request ID. Do not block
the ACP dispatch loop waiting for peer traffic; use a spawned task or the
connection's application future.

The final response is the MCP result directly, including its `resultType`, or
the original MCP error. For MRTR, process the `input_required` result and send
a fresh request with `inputResponses` and the exact opaque `requestState`.

Discovery reports only the MCP revision exposed by this binding, even if the
hosted backend also supports older revisions through other transports.

## Subscriptions and cancellation

`subscriptions/listen` keeps one request alive. Its acknowledgement and updates
arrive as request-scoped notifications, with the logical request ID in
`io.modelcontextprotocol/subscriptionId`. An unrelated tool call does not share
that subscription's state or lifetime.

Use `SentRequest::cancel` (or drop an unconsumed request) to cancel the outer
ACP operation. The provider stops that operation's backend work and returns a
result or cancellation error. Removing a provider stops its outstanding work;
no separate `mcp/disconnect` exchange exists.

## Resource limits and remaining work

The native provider admits at most 64 concurrent operations per declared
server and checks a 16 MiB serialized payload limit before starting work or
forwarding backend responses/notifications. Rejected work reports an error;
completion and cancellation release the admission slot.

These are not end-to-end memory bounds. The public SDK `Channel` and outgoing
queues remain unbounded. A bounded native transport path is still required
before stabilization; admission and per-message size checks do not prevent
accumulation behind a slow peer. The [HTTP adapter](./mcp-bridge.md) separately
bounds its own response queues and fails/cancels an overflowing operation.

## Runnable example

```sh
cargo run -p agent-client-protocol-rmcp \
  --example stateless_native_mcp \
  --features unstable_mcp_over_acp,unstable_protocol_v2
```

This direct ACP example uses actual rmcp tools without the HTTP polyfill.
See the [protocol reference](./protocol.md#native-mcp-over-acp) for wire details
and the [RFD](https://agentclientprotocol.com/rfds/mcp-over-acp) for the design.
