# Native MCP-over-ACP

The native binding exposes **MCP 2026-07-28** over an existing ACP connection.
Enable `unstable_mcp_over_acp`; draft ACP v2 also requires
`unstable_protocol_v2`.

## Services and operations

Attach an `mcp_server::McpServer` through the existing global or session builder
APIs. Session setup advertises an opaque `serverId`. Each `mcp/message` request
addresses that server directly; there is no MCP connect/disconnect exchange,
initialization handshake, or discovery prerequisite.

`McpService` is a reusable application service. Its `execute` method receives a
`McpRequest` and a request-scoped `McpRequestContext`. The context exposes:

- The declared server ID and logical request ID.
- The session and tool call that the agent says the request was sent for,
  when it supplies them.
- Validated MCP metadata, including version and client capabilities.
- Caller cancellation and operation cancellation caused by removal or shutdown.
- An operation-scoped notification sender.
- The host ACP connection for application tools.

Share caches, tool catalogs, and database pools deliberately. Do not infer a
request's identity or capabilities from a previous call.

`session_id()` and `tool_call_id()` return the request's `sessionId` and
`toolCallId`. `McpConnectionTo` has the same accessors, so a `tool_fn` closure
can read them from its connection. Both are `None` when the agent sent none,
for example for a request it sends during session setup. A child session's
work carries the child's ID.

The SDK does not check these values. Before a tool uses the session to choose
what it acts on, check that the session is active on the connection, or is a
child the agent announced for an active session, and that this server was
declared to it or to one of its ancestors. Do not reject or delay a request
because you have not seen its tool call. The agent can report the call
afterwards. A service cannot yet reject a request with `-32602`: an error it
returns is reported as a backend failure (`-33002`). See the
[RFD](https://agentclientprotocol.com/rfds/mcp-over-acp#session-and-tool-call-attribution).

Use `McpServer::new_service` for native-only execution, or
`new_service_with_standalone` to also serve a separate standalone transport.
The connector-based constructor remains an adapter for backends that need a
fresh component per operation.

The rmcp builder and `from_rmcp` use a shared native service and rmcp's
one-request transport. Standalone serving retains its ordinary MCP lifecycle.
Scoped `tool_fn` and `tool_fn_mut` closures remain supported; cancelling a call
must not poison the runner or start already-cancelled queued work.

## Calling a server

Use `MessageMcpRequest::new(server_id, request_id, method)`. Choose a fresh
logical ID for every operation and include these inner parameters:

```json
{
  "_meta": {
    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
    "io.modelcontextprotocol/clientCapabilities": {}
  }
}
```

The logical ID stays unchanged through proxies and becomes the inner MCP
JSON-RPC ID. It is separate from the outer ACP ID, which can change on each hop.

When the request is for a session's work, name the session with
`.session_id(...)`, and add `.tool_call_id(...)` for the tool call you report in
`session/update`. Use the IDs you send on `session/update` for that work, so a
child session's request carries the child's ID. Omit a value you do not know,
and do not send a tool call without its session. A provider can reject a
request that has no session.

Register a `MessageMcpNotification` handler before starting an operation that
streams notifications. Route notifications by server and logical request ID.
Use spawned work or the application's connection future to wait for peer
traffic; do not block the ACP dispatcher.

## Results and errors

First handle the outer ACP result, then match `MessageMcpResponse`:

- `Result { result, .. }` contains an opaque MCP result, including JSON null,
  metadata, and MRTR's `input_required` result.
- `Error { error, .. }` contains an MCP error. Preserve its numeric code, data
  omission versus explicit null, and unknown extensions.
- An outer ACP error describes a binding failure, not an MCP method outcome.

An inner MCP `-32000` is not ACP authentication-required. Tool execution
failures with `isError` remain MCP results. For MRTR, retry with a fresh logical
ID, current metadata, input responses, and the unchanged opaque request state.

ACP v1 and v2 have independent carrier/error types. Their wire representations
currently agree; use the types for the negotiated ACP version.

## Cancellation and cleanup

A subscription is one long-running request, not shared connection state.
Acknowledgments and updates carry the logical request ID in
`_meta["io.modelcontextprotocol/subscriptionId"]`.

ACP cancellation is best effort on the wire. The Rust SDK revokes an owned
operation's notification rights, cancels its work, and joins supported cleanup.
The logical ID remains active until cleanup finishes; completion may win a
cancellation race.

Mutable function tools serialize user execution, not cancellation of queued work.
A cancelled queued call is destroyed without entering its closure or waiting for
another registration's active call. Recoverable session-local runner failures close
only that session's registrations; they do not seal connection-wide admission.

Custom services must observe `operation_cancellation()` and return only after
owned cleanup finishes. This also applies when their registration is removed,
the ACP input closes, or the application foreground completes. The SDK keeps
native supervisors and scoped runners driven during that cleanup. It cannot
roll back arbitrary detached application work.

## Example and scope

```sh
cargo run -p agent-client-protocol-rmcp \
  --example stateless_native_mcp \
  --features rmcp_3,unstable_mcp_over_acp
```

See the [wire reference](./protocol.md#native-mcp-over-acp),
[HTTP re-export](./mcp-bridge.md), and
[migration guide](./migration-stateless-mcp.md).

This change does not redesign generic ACP queues, frame types, task admission,
or byte accounting. Native MCP protocol state is request-scoped; that is not a
claim of bounded total process memory or complete optional MCP conformance.
