# MCP-over-ACP Compatibility Bridge

**Draft checkpoint:** The stateful adapter described here targets older MCP
semantics. It is not an implementation of MCP 2026-07-28, which removes
initialization, protocol sessions, GET, and DELETE. The intended MCP-over-ACP
transport will target that stateless revision only; retaining this session mode
for backwards compatibility is not a goal. See the
[modernization audit](https://agentclientprotocol.com/rfds/mcp-over-acp#modernization-audit-mcp-2026-07-28).

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

The polyfill supports stable protocol v1 by default. To place it in a draft-v2
conductor chain, enable `unstable_protocol_v2` on both the conductor and
polyfill dependencies:

```toml
agent-client-protocol-conductor = { version = "...", features = ["unstable_protocol_v2"] }
agent-client-protocol-polyfill = { version = "...", features = ["unstable_protocol_v2"] }
```

The feature makes this concrete compatibility proxy recognize v2
initialization, capability, session setup, and `mcp/*` wire types. It does not
change the core attachment API. `Proxy.v2().with_mcp_server(...)` provides
connection-global attachment. `V2SessionBuilder::with_mcp_server(...)` and
`V2ResumeSessionBuilder::with_mcp_server(...)` provide per-session attachment
for new and resumed sessions respectively. With `unstable_session_fork`,
`V2ForkSessionBuilder::with_mcp_server(...)` provides per-session fork
attachment. The polyfill adapts their native declarations when the final agent
supports only HTTP MCP.

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
`agent_client_protocol::mcp_server::McpServer`. The SDK advertises it in session
setup requests as `McpServer::Acp`; callers do not need to construct a transport
placeholder themselves. In v2, `Proxy.v2().with_mcp_server(...)` provides
connection-global attachment. `V2SessionBuilder::with_mcp_server(...)` and
`V2ResumeSessionBuilder::with_mcp_server(...)` provide per-session attachment
for new and resumed sessions respectively. With `unstable_session_fork`,
`V2ForkSessionBuilder::with_mcp_server(...)` provides per-session fork
attachment. The polyfill translates those native declarations at the final
compatibility boundary.

During initialization, the polyfill forwards the request to its successor. When
the successor advertises HTTP MCP support, the polyfill advertises native ACP
MCP support in the response seen upstream:

- v1 sets `agentCapabilities.mcpCapabilities.acp` to `true`.
- v2 adds the `capabilities.session.mcp.acp` marker.

In this chain position that capability means the chain can consume native
MCP-over-ACP declarations through the adapter; it does not imply that the final
agent implements the transport itself.

If the successor already advertises native ACP MCP support, the polyfill leaves
the capability, declarations, and `mcp/message` traffic unchanged. If it
supports neither native nor HTTP MCP, the polyfill does not advertise ACP MCP
support and rejects any native declaration that is nevertheless supplied.

## Transformation

For each schema-selected `McpServer::Acp` entry in a session setup request, the
polyfill:

1. Creates or reuses a connection-scoped localhost bridge endpoint for the
   `serverId` and replaces the declaration with the HTTP transport for the
   final agent.
2. Retains the native `serverId` so connections can be routed back to the
   component that provided the server.
3. Opens a native connection when an HTTP MCP client initializes a logical
   session, sending `mcp/connect` with that server ID toward the provider.
   Independent HTTP sessions receive independent native connections.
4. Relays requests and notifications through `mcp/message`, using the returned
   `connectionId` for that active MCP connection.
5. Sends an `mcp/disconnect` request when the logical HTTP MCP session closes
   and removes that connection from the bridge. The listening endpoint remains
   available for other sessions.

Enable the polyfill crate's `unstable_session_fork` feature when adapting fork
requests. Stable v1 setup includes `session/new`, `session/load`, and
`session/resume`; draft v2 includes `session/new` and `session/resume`. Both
versions include `session/fork` when `unstable_session_fork` is enabled.

Declarations using another transport are left unchanged, including extension
transports represented by v2's `McpServer::Other`.

Endpoints are cached by `serverId` across session setup requests on the ACP
connection. The output declaration is rebuilt for each occurrence, preserving
that occurrence's `name`, `_meta`, and other unmodified extension fields even
when its endpoint is reused.

The native wire envelopes are documented in the [SDK Protocol
Reference](./protocol.md#native-mcp-over-acp).

## HTTP Mode

`McpOverAcpPolyfill::http()` is the default compatibility shape. It replaces
the native declaration with an HTTP MCP URL at `http://127.0.0.1:PORT`. The
embedded server accepts MCP POST requests, SSE GET streams, and session DELETE
requests at `/`, retaining JSON-RPC batch frames and correlating each POST with
its response.

The adapter uses stateful Streamable HTTP. A successful MCP initialization
returns an `MCP-Session-Id` header. Clients must send that header on subsequent
POST, GET, and DELETE requests; unknown or closed sessions return HTTP 404.
Clients that previously ignored session headers must retain the returned ID.
An individual POST response or GET stream closing does not end the session.

```rust,ignore
let bridge = McpOverAcpPolyfill::http();
```

The listener is bound only on loopback and uses an ephemeral port. It does not
implement resumable SSE event IDs.

## Lifecycle and Failure Behavior

The listener and the logical MCP connections have different lifetimes. Endpoint
creation alone does not open an MCP connection. Each HTTP MCP session receives
its own native `connectionId` from `mcp/connect`, so initialization, request IDs,
and server-originated messages cannot cross between clients. Disconnecting one
session leaves its siblings and the cached endpoint usable.

Deleting an HTTP MCP session stops its local transport and sends
`mcp/disconnect` for that session's native connection. Request failures use the
corresponding request's error path; notifications are never answered with
synthetic errors. Closing the parent ACP connection drops its listeners and
session tasks; a disconnect exchange is not possible after that transport is
gone.

A reverse `mcp/message` request for an unknown `connectionId` receives
`Invalid params`. A reverse notification for an unknown connection is ignored,
as required for JSON-RPC notifications.

The polyfill does not infer or store ACP session IDs. Association is carried by
the declared `serverId` and the resulting active `connectionId`.

Known checkpoint gaps: aborting HTTP initialization before receiving its
response can leave an unadvertised session until the listener stops. There is
no idle timeout, and DELETE during pending forward/reverse requests still
needs regression coverage. These are reasons to keep the checkpoint in draft,
not features to preserve in the stateless replacement.
