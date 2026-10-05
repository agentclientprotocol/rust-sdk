# Native MCP-over-ACP

The native binding exposes **MCP 2026-07-28** over an existing ACP connection.
Enable `unstable_mcp_over_acp`; draft ACP v2 also requires
`unstable_protocol_v2`. The SDK uses the released schema 1.10.1.

## Services and operations

Attach an `mcp_server::McpServer` through the existing global or session builder
APIs. Session setup advertises an opaque `serverId`. Each `mcp/message` request
addresses that server directly; there is no MCP connect/disconnect exchange,
initialization handshake, or discovery prerequisite.

`McpService` is a reusable application service. Its `execute` method receives a
`McpRequest` and a request-scoped `McpRequestContext`. The context exposes:

- The declared server ID and logical request ID.
- Validated MCP metadata, including version and client capabilities.
- Caller cancellation and operation cancellation caused by removal or shutdown.
- An operation-scoped notification sender.
- The host ACP connection for application tools.

Share caches, tool catalogs, and database pools deliberately. Do not infer a
request's identity or capabilities from a previous call.

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
  --features unstable_mcp_over_acp,unstable_protocol_v2
```

See the [wire reference](./protocol.md#native-mcp-over-acp),
[HTTP re-export](./mcp-bridge.md), and
[migration guide](./migration-stateless-mcp.md).

This change does not redesign generic ACP queues, frame types, task admission,
or byte accounting. Native MCP protocol state is request-scoped; that is not a
claim of bounded total process memory or complete optional MCP conformance.
