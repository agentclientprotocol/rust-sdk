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

1. Creates or reuses one connection-scoped loopback listener and replaces the
   declaration with an HTTP URL whose path encodes the non-secret `serverId`.
   No per-server listener or route-table entry is allocated.
2. Routes each request back to the component that owns that native registration.
   The provider, not possession of the URL, decides whether it still exists.
3. Adds a runtime-only bearer credential derived from the connection secret and
   server ID to the HTTP declaration's headers. The endpoint authenticates and
   checks supplied Origin headers before reading the request body. Credentials
   never appear in URLs; an ephemeral port alone is not access control.
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

The same server ID derives the same route and credential on this ACP connection.
The output declaration is rebuilt for each occurrence, preserving its `name`,
`_meta`, and other unmodified extension fields. Failed setup and declaration
churn cannot accumulate per-server endpoint allocations. A server ID must never
be rebound to a different registration during the connection's lifetime.

The native wire envelopes are documented in the [SDK Protocol
Reference](./protocol.md#native-mcp-over-acp).

## HTTP Mode

`McpOverAcpPolyfill::http()` is the default compatibility shape. It replaces
the native declaration with an HTTP MCP URL at `http://127.0.0.1:PORT/<route>`. The
embedded server accepts a single JSON-RPC request per POST at that route, returning
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

Each POST owns a pending native request, not an MCP session. Closing its response
stream cancels that request. A terminal outcome ends native work, but HTTP
admission remains held until the response body is consumed or dropped. The
listening endpoint remains available for later requests; releasing the native
registration makes requests through its old URL fail rather than reviving it.

The adapter limits each response's queued notifications to 16 messages and
256 KiB of serialized data, admits at most 64 HTTP responses at a time, and caps
request bodies and terminal payloads at 1 MiB. The body owns the admission permit,
including while a client is not reading. A separate terminal-response path
avoids stranding completion behind a full queue. Overflow explicitly fails and
cancels that operation without blocking the shared runner or dropping events silently.

The bridge unwraps the ACP outcome carrier before creating the HTTP JSON-RPC
response. MCP error codes/data stay MCP errors; binding failures use their
separate error codes. Queued notifications precede the terminal response.

Unknown or late provider notifications are ignored; reverse MCP requests are
not supported. The adapter does not infer ACP session IDs or maintain MCP
initialization state.

## Native-tool re-export contract

The adapter creates a **new HTTP endpoint for native tool semantics**. It does
not preserve another HTTP gateway's parameter-header routing or authorization.
It removes transport-only `x-mcp-header` annotations from actual schema positions
in `tools/list` results. Argument schemas and validation keywords, tool ordering,
pagination, metadata, and similarly named properties/example/default data remain
unchanged. Annotated native tools remain listed and callable.

Each `tools/call` issues exactly one native call, without hidden descriptor reads
or a prior client `tools/list` requirement. Native passthrough does not transform
the original descriptors. `Mcp-Param-*` headers are rejected; they confer no
authority on this endpoint. Standard MCP method/name/version header checks remain.

If a deployment depends on an existing HTTP gateway's mirrored-parameter policy,
it must implement that policy at this endpoint or decline this re-export.

## Validation scope

This does not establish every optional MCP feature or complete HTTP conformance.
In particular, HTTP response limits alone do not prove native transport bounds.
Owned operation cleanup and end-to-end bounded transport are stabilization gates;
see [Native MCP-over-ACP](./mcp-over-acp.md).
