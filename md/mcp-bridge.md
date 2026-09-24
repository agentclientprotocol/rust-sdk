# Stateless MCP-over-ACP HTTP Adapter

`agent-client-protocol-polyfill::mcp_over_acp::McpOverAcpPolyfill` adapts the
native ACP MCP transport for a final agent with an MCP 2026-07-28 HTTP client.
MCP adaptation is explicit and is not built into the conductor. There is no
fallback to older MCP revisions.

The component-facing side of the bridge always uses the opt-in native protocol:

- Servers are declared as `McpServer::Acp` with a `serverId`.
- Each operation uses `mcp/message` with `serverId` and a logical `requestId`.
- The provider sends notifications for that operation; the final ACP response
  carries its MCP result or error.
- ACP request cancellation stops only that operation. There is no MCP
  initialize/connect/disconnect or session-header exchange.

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
3. Adds a runtime-only bearer credential to the HTTP declaration. The endpoint
   requires that credential and checks supplied Origin headers; an ephemeral
   port alone is not access control.
4. For each POST, allocates a unique logical MCP request ID and sends
   `mcp/message` to the provider. Two HTTP clients may use the same external
   JSON-RPC ID without sharing routing or state.
5. Relays notifications and a final result/error for that request. Closing
   its HTTP response cancels the corresponding ACP request, not the listener.

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
embedded server accepts a single JSON-RPC request per POST at `/`, returning
JSON for a terminal-only response or SSE for a request that emits notifications.
GET and DELETE return 405. Batches and client-originated JSON-RPC responses
are rejected; there is no standalone GET event stream or MCP session ID.

```rust,ignore
let bridge = McpOverAcpPolyfill::http();
```

Clients must send the bearer header from the declaration, both JSON and SSE
Accept types, and the required MCP protocol-version, method, and applicable
name headers. Mirrored names support MCP's Base64 sentinel encoding. Missing,
duplicate, or mismatched routing headers are rejected.

The listener is bound only on loopback. Resumable SSE event IDs are not part of
the target MCP revision. Subscription IDs inside
`_meta["io.modelcontextprotocol/subscriptionId"]` are translated back to the
HTTP request's original ID in notifications and graceful completion results;
other metadata, progress tokens, and opaque retry state are not rewritten.

## Lifecycle and Failure Behavior

Each POST owns a pending native request, not an MCP session. A terminal result,
error, response-stream close, or overflow removes that request's routing state.
The listening endpoint remains available for later requests.

The adapter limits each response's queued notifications to 16 messages and
256 KiB of serialized data, with 64 active requests and 32 listening endpoints
per adapter. A separate terminal-response path avoids stranding completion
behind a full queue. Overflow explicitly fails and cancels that operation
without blocking the shared runner or dropping events silently.

Unknown or late provider notifications are ignored; reverse MCP requests are
not supported. The adapter does not infer ACP session IDs or maintain MCP
initialization state.

## Remaining scope

Tools using `x-mcp-header` annotations are currently unsupported and fail
closed: they are omitted from listings, calls are rejected, and supplied
`Mcp-Param-*` headers are rejected. For a direct tool call the adapter fetches
the tool descriptor internally, including pagination, so the caller does not
need a prior tools/list handshake. That lookup is an explicit per-call cost.

This is not yet full HTTP conformance. Native SDK `Channel` and outgoing
queues also remain unbounded; the HTTP queue limits above do not establish
end-to-end native backpressure. Native admission/payload limits and the
remaining transport work are described in [Native MCP-over-ACP](./mcp-over-acp.md).
