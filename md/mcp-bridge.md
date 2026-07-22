# MCP-over-ACP Compatibility Bridge

`agent-client-protocol-polyfill::mcp_over_acp::McpOverAcpPolyfill` adapts the
native ACP MCP transport for a final agent that accepts HTTP MCP
servers. MCP adaptation is explicit and is not built into the conductor.

The component-facing side of the bridge always uses the opt-in native protocol:

- Servers are declared as `McpServer::Acp` with a `serverId`.
- Connections use `mcp/connect`, `mcp/message`, and `mcp/disconnect`.
- `mcp/disconnect` is a request with a response.

The SDK-local underscore-prefixed method family and HTTP declarations with a
special URL scheme have been retired. The polyfill now translates native
declarations to real localhost HTTP URLs only at
the compatibility boundary.

Native MCP-over-ACP requires the core SDK's `unstable_mcp_over_acp` feature. The
polyfill enables that feature on its core dependency, so applications using the
polyfill receive it through Cargo feature unification.

## Placement

Insert the polyfill immediately before the final agent that lacks native
MCP-over-ACP support:

```rust,ignore
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_polyfill::mcp_over_acp::McpOverAcpPolyfill;

let components = ProxiesAndAgent::new(agent)
    .proxy(application_proxy)
    .proxy(McpOverAcpPolyfill::http());

ConductorImpl::new_agent("conductor", components)
    .run(upstream_transport)
    .await?;
```

The application proxy can attach a high-level
`agent_client_protocol::mcp_server::McpServer`. The SDK advertises it in
session setup requests as `McpServer::Acp`; callers do not need to construct a
transport placeholder themselves.

During initialization, the polyfill forwards the request to its successor and
sets `agentCapabilities.mcpCapabilities.acp` in the response seen upstream when
the successor advertises HTTP MCP support. In this chain position that
capability means the chain can consume native MCP-over-ACP declarations through
the adapter; it does not imply that the final agent implements the transport
itself.

If the successor already advertises native ACP MCP support, the polyfill leaves
the capability, declarations, and `mcp/message` traffic unchanged. If it
supports neither native nor HTTP MCP, the polyfill does not advertise ACP MCP
support and rejects any native declaration that is nevertheless supplied.

## Transformation

For each `McpServer::Acp` entry in `session/new`, `session/load`,
`session/resume`, or feature-gated `session/fork`, the polyfill:

1. Creates or reuses a connection-scoped localhost bridge endpoint for the
   `serverId` and replaces the declaration with the HTTP transport for the
   final agent.
2. Retains the native `serverId` so connections can be routed back to the
   component that provided the server.
3. Opens the endpoint's native connection by sending `mcp/connect` with that
   server ID toward the provider.
4. Relays requests and notifications through `mcp/message`, using the returned
   `connectionId` for that active MCP connection.
5. Sends an `mcp/disconnect` request when the local transport closes and removes
   the connection from the bridge.

Enable the polyfill crate's `unstable_session_fork` feature when adapting fork
requests.

Endpoints are cached by `serverId` across session setup requests on the ACP
connection. The output declaration is rebuilt for each occurrence, preserving
that occurrence's `name` and `_meta` even when its endpoint is reused.

The native wire envelopes are documented in the [SDK Protocol
Reference](./protocol.md#native-mcp-over-acp).

## HTTP Mode

`McpOverAcpPolyfill::http()` is the default compatibility shape. It replaces
the native declaration with an HTTP MCP URL at `http://127.0.0.1:PORT`. The
embedded server accepts MCP POST requests and an SSE GET stream at `/`, retaining
JSON-RPC batch frames and correlating each POST with its response.

```rust,ignore
let bridge = McpOverAcpPolyfill::http();
```

The listener is bound only on loopback and uses an ephemeral port. It does not
implement resumable SSE event IDs.

## Lifecycle and Failure Behavior

Each bridge endpoint receives a unique `connectionId` from `mcp/connect`. The
polyfill keeps a connection map until the endpoint's transport task closes,
then removes the entry, sends `mcp/disconnect`, and observes its response.
Request failures use the corresponding request's error path; notifications are
never answered with synthetic errors.

A reverse `mcp/message` request for an unknown `connectionId` receives
`Invalid params`. A reverse notification for an unknown connection is ignored,
as required for JSON-RPC notifications.

The polyfill does not infer or store ACP session IDs. Association is carried by
the declared `serverId` and the resulting active `connectionId`.
