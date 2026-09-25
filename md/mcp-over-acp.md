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

`McpService` is a reusable application service. Each `execute` call owns one
operation future and receives an `McpRequestContext` with `server_id()`,
`request_id()`, validated `metadata()`, cancellation, and an async
`send_notification` method. Share tool implementations, caches, and connection
pools deliberately; never infer a request's identity or capabilities from a
previous operation.

Use `McpServer::new_service` for a native service, or
`new_service_with_standalone` when also exposing an independent standalone
transport. The connector-based factory remains an explicit adapter for backends
that require per-operation construction; stateless MCP does not require it.

The rmcp integration's builder and `from_rmcp` use the reusable service path
for ACP attachments. Each operation uses rmcp's direct, one-request transport
without `initialize`. Its wrapper supervises rmcp handler futures through
cancellation and cleanup instead of merely dropping detached task handles.

Custom `McpService` implementations must observe `operation_cancellation()` and
return only after their owned cleanup finishes. The binding waits for this
completion; it cannot forcibly terminate detached application work.

The scoped `tool_fn` helpers continue to provide `McpConnectionTo` for host ACP
access. For decisions using the full MCP metadata/capabilities, implement
`McpService` or an rmcp handler receiving its `RequestContext`. Standalone MCP
connections have no ACP server or logical request ID.

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

The final successful ACP response is `MessageMcpResponse::Result { result, .. }`
or `MessageMcpResponse::Error { error, .. }`. Match that carrier before interpreting
the MCP outcome. The result preserves all MCP fields, including `resultType`;
the error preserves its MCP code, message, optional data, and extensions.
An MCP code must never be treated as an ACP code: for example, inner `-32000`
does not mean ACP authentication is required.

Outer ACP failures instead describe invalid binding input, cancellation,
resource exhaustion, an unavailable registration, or a failed backend/transport.
For MRTR, process the inner `input_required` result and send a fresh request
with `inputResponses` and the exact opaque `requestState`.

Discovery reports only the MCP revision exposed by this binding, even if the
hosted backend also supports older revisions through other transports.

## Subscriptions and cancellation

`subscriptions/listen` keeps one request alive. Its acknowledgement and updates
arrive as request-scoped notifications, with the logical request ID in
`io.modelcontextprotocol/subscriptionId`. An unrelated tool call does not share
that subscription's state or lifetime.

Use `SentRequest::cancel` (or drop an unconsumed request) to cancel the outer
ACP operation. The provider revokes output immediately and stops that operation's
owned backend work; its admission slot and logical ID remain held until cleanup
finishes. Cancellation produces an outer cancellation error unless completion
already won the race. Removing a registration or receiving transport EOF cancels
its outstanding work; no separate `mcp/disconnect` exchange exists.

## Resource limits and remaining work

The native binding has per-registration admission and serialized payload limits.
Resource exhaustion is an outer `MCP_RESOURCE_EXHAUSTED` (`-33000`) failure, not
ACP authentication and not an inner MCP tool error.

The transport revision introduces finite `ConnectionLimits` and `BudgetedFrame`
ownership. Adapters must keep the frame's permit through staging, deferred
dispatch, and writes; extracting a payload must not silently release its charge
while retaining the data. Async producers await capacity; synchronous dispatch
must fail explicitly instead of blocking the dispatcher needed to free capacity.

The same item-limit policy currently governs frame queues, pending requests,
running tasks, dynamic handlers, and deferred dispatch; the default is 32.
The shared payload budget defaults to 64 MiB with a 16 MiB frame maximum and
reserved response/cancellation capacity. These are serialized-payload charges,
not an exact bound on total process memory or allocations inside user code.

Regression coverage includes sender-clone saturation, cross-budget forwarding,
retained responses and callbacks, EOF draining, and cancellation while cleanup is
paused. The [HTTP adapter](./mcp-bridge.md) separately owns its response-body permits
and fails/cancels overflowing operations. Full MCP conformance and protocol
stabilization remain separate from this implementation evidence.

## Runnable example

```sh
cargo run -p agent-client-protocol-rmcp \
  --example stateless_native_mcp \
  --features unstable_mcp_over_acp,unstable_protocol_v2
```

This direct ACP example uses actual rmcp tools without the HTTP polyfill.
See the [protocol reference](./protocol.md#native-mcp-over-acp) for wire details
and the [RFD](https://agentclientprotocol.com/rfds/mcp-over-acp) for the design.
The [migration guide](./migration-stateless-mcp.md) lists the breaking changes.
