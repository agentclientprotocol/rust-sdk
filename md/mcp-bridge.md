# Request-scoped MCP HTTP Re-export

`agent-client-protocol-polyfill::mcp_over_acp::McpOverAcpPolyfill` adapts the
native ACP MCP transport for a final agent with an MCP 2026-07-28 HTTP client.
MCP adaptation is explicit and is not built into the conductor. There is no
fallback to older revisions.

The component-facing side of the bridge always uses the opt-in native protocol:

- Servers are declared as `McpServer::Acp` with a `serverId`.
- Each operation uses `mcp/message` with the server ID and a fresh logical ID.
- Provider notifications belong to that operation; its final response carries
  an inner MCP result or error.
- There is no initialize/connect/disconnect or MCP session-header exchange.

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

1. Creates or reuses one connection-scoped loopback listener.
2. Replaces the declaration with a server-addressed URL and an HMAC-derived
   Bearer token in an Authorization header, preserving name and metadata.
3. Maps each HTTP POST to one independent `mcp/message` request with a fresh
   UUID logical ID. External HTTP IDs may overlap without sharing lifetime.
4. Streams that operation's notifications and projects its terminal carrier
   into the corresponding HTTP JSON-RPC response.
5. Cancels only that operation if its HTTP response stream is dropped.

Enable the polyfill crate's `unstable_session_fork` feature when adapting fork
requests. Stable v1 setup includes `session/new`, `session/load`, and
`session/resume`; draft v2 includes `session/new` and `session/resume`. Both
versions include `session/fork` when `unstable_session_fork` is enabled.

Declarations using another transport are left unchanged, including extension
transports represented by v2's `McpServer::Other`.

Routes are derived from server IDs, not accumulated as per-server listeners or
route-table entries. The token authenticates that server on this ACP connection.
Keep the declaration's header; never place its bearer credential in the URL.

The native wire envelopes are documented in the [SDK Protocol
Reference](./protocol.md#native-mcp-over-acp).

## HTTP Mode

`McpOverAcpPolyfill::http()` replaces native declarations with loopback HTTP
URLs. Each POST carries current MCP method/version headers and required inner
version/client-capabilities metadata. Results use JSON, or an operation-scoped
SSE response when notifications are emitted.

```rust,ignore
let bridge = McpOverAcpPolyfill::http();
```

Authentication and Origin checks precede bounded request parsing. The adapter
does not accept batches, client responses, MCP initialize/session headers, or
standalone GET/DELETE streams. It has no resumable SSE event IDs.

### Native-tool semantics, not another HTTP gateway's policy

The endpoint re-exports native tools with its own routing and authentication.
It strips transport-only `x-mcp-header` annotations from schema positions in
`tools/list`, retaining argument validation, property names, defaults, examples,
and other instance data. Annotated native tools remain listed and callable.

`tools/call` is forwarded directly, without hidden descriptor discovery.
`Mcp-Param-*` headers are rejected and have no authorization or routing meaning.
An external HTTP gateway's mirrored-header security policy is not transported;
deployments relying on it must enforce that policy separately at the new endpoint.

## Lifecycle and Failure Behavior

Previously queued notifications precede the terminal outcome. Only a matching
namespaced subscription ID is rewritten to the external HTTP ID; opaque retry
state, unrelated metadata, and progress tokens are untouched.

Inner MCP errors retain their code, data, and extensions. Outer ACP failures are
projected as binding failures, not mistaken for MCP method outcomes.

Incoming request bodies retain the previous bridge's 2 MiB parsing limit.
Each operation has a finite 128-message notification queue. If its receiver closes
or the queue fills, only that operation is stopped; an open SSE stream ends without
a terminal outcome rather than silently losing notifications or blocking other
operations. The adapter does not impose an operation-count admission limit,
notification-byte budget, or terminal-response size limit. These contracts do not
establish a bound on total application memory.

The polyfill does not infer ACP session IDs. Native registration dispatch remains
the authority for whether a server exists. The listener and signing secret live
only for the containing ACP connection.
