# Native MCP-over-ACP

**Draft checkpoint:** This chapter describes the in-progress connection-oriented
implementation, not conformance with MCP 2026-07-28. The stabilization target is
that stateless MCP revision only, with no legacy compatibility requirement.
Its initialization and connect/disconnect API below will need replacement; see
the [modernization audit](https://agentclientprotocol.com/rfds/mcp-over-acp#modernization-audit-mcp-2026-07-28).

MCP-over-ACP lets an ACP client or proxy provide an MCP server through its
existing ACP connection. A native agent can use that server without starting
another process, opening an HTTP endpoint, or introducing a conductor.

Enable `unstable_mcp_over_acp` on `agent-client-protocol`. Add
`unstable_protocol_v2` for draft-v2 connections. The wire types come from the
shared ACP schema; the transport remains unstable.

## Providing a server

The `mcp_server::McpServer` APIs attach servers to session setup requests using
`McpServer::Acp` declarations. The separate `agent-client-protocol-rmcp` crate
can build a server from tools or an `rmcp` service without making `rmcp` a
dependency of the core SDK.

There are three distinct identifiers:

| Identifier | Meaning |
| --- | --- |
| `serverId` | Provider-generated identity for the declared MCP server |
| `connectionId` | Provider-generated identity for one active connection to that server |
| Outer JSON-RPC `id` | Identity of one ACP request, including an `mcp/message` request |

A server can accept multiple connections. Each has independent MCP
initialization, pending requests, and shutdown. Providers must not reuse a
server ID for different servers visible on the same ACP connection.

Servers are ready as soon as their declarations are published. An agent can
connect and run MCP initialization before returning the ACP session ID.
Providers must not wait for the session setup response before serving MCP.

## Consuming a server

The core `mcp_client` module supplies `McpOverAcp`, a server transport for an
ordinary MCP client. Its version-specific connection helpers open a native
connection to the declared server and route bidirectional MCP traffic over
the ACP channel. No HTTP adapter is involved.

The consuming ACP agent holds a `ConnectionTo<Client>` (or its v2 counterpart):
the ACP client is providing the MCP server. The returned transport implements
`ConnectTo<role::mcp::Client>`, so it can be used by the SDK's MCP client role
or connected to an external MCP implementation through a byte-stream adapter.

The helper opens the transport, not the MCP protocol session. The MCP client
still performs its normal `initialize` / `notifications/initialized` handshake.
It can then list and call tools, while also handling server-originated requests
and notifications.

Do connection setup and MCP work outside the ACP dispatch loop, for example
in a connection-spawned task. Awaiting a peer response inside an ACP message
handler can block the very messages needed to complete that operation. See
[Ordered Application Dispatch](./ordered-application-dispatch.md).

## Closing connections

Use the helper's awaited close operation when shutdown completion matters.
Dropping a native consumer schedules best-effort disconnect; it cannot report
whether the provider acknowledged cleanup.

The provider acknowledges `mcp/disconnect` after stopping that connection's
relay and server work. Requests already dispatched to the child are completed
or failed; this checkpoint still needs explicit failure of requests queued in
the relay at shutdown and direct handler-drop coverage. Other MCP connections
to the same server, and the containing ACP connection, stay usable.

ACP transport closure drops its MCP connection-scoped work. No disconnect
exchange is possible once the ACP transport is gone.

## Runnable direct example

From the repository root:

```sh
cargo run -p agent-client-protocol-rmcp \
  --example native_mcp_over_acp \
  --features native_mcp_example
```

The example connects an ACP client directly to an ACP agent, attaches an
`rmcp` server on the client side, and uses a real MCP client on the agent side.
It exercises the normal MCP handshake and tool traffic without a conductor,
an HTTP listener, or a subprocess.

## Compatibility

An agent advertises native support with
`agentCapabilities.mcpCapabilities.acp: true` in v1, or
`capabilities.session.mcp.acp: {}` in draft v2. Do not advertise support unless
the agent can consume the transport.

For an HTTP-capable agent without native support, use the explicit
[MCP-over-ACP compatibility bridge](./mcp-bridge.md). Its listening endpoint
can be shared, but each logical HTTP MCP session has its own native connection.

See the [protocol reference](./protocol.md#native-mcp-over-acp) for exact
wire envelopes and the [RFD](https://agentclientprotocol.com/rfds/mcp-over-acp)
for the protocol design.
