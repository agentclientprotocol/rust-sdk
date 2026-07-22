# MCP-over-ACP Compatibility Bridge

`agent-client-protocol-polyfill::mcp_over_acp::McpOverAcpPolyfill` is an
explicit proxy for agents that cannot consume the SDK's legacy ACP-routed MCP
server declarations directly. MCP adaptation is no longer built into the
conductor.

## Native and Legacy Transports

Two similarly named mechanisms coexist and must not be mixed:

- The draft **native** transport is enabled by `unstable_mcp_over_acp`. It uses
  `McpServer::Acp` plus `mcp/connect`, `mcp/message`, and `mcp/disconnect`.
- The polyfill's **legacy compatibility** path recognizes
  `McpServer::Http` entries whose URL begins with `acp:`. It routes them with
  `_mcp/connect`, `_mcp/message`, and `_mcp/disconnect`.

The polyfill currently implements the second path. New implementations that
control both peers should prefer the draft native schema types; use the
polyfill only where compatibility requires it.

## Placement

Insert the polyfill as a proxy before the final agent:

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

The application proxy can then supply a legacy declaration such as an HTTP MCP
server whose URL is `acp:server-1`. The identifier is opaque and must route back
to the component that provides the MCP server.

During initialization, the polyfill forwards the request to its successor and
sets `agentCapabilities.mcpCapabilities.acp` in the response seen upstream. In
this chain position that capability means the polyfill can handle the routed
transport; it does not imply that the final agent implements it natively.

## Transformation

For each `McpServer::Http` entry with an `acp:` URL in `session/new`, the
polyfill:

1. Rejects non-empty HTTP headers, which have no defined meaning for this
   compatibility transport.
2. Reuses an existing listener for the same ACP identifier or binds a new
   localhost TCP listener.
3. Replaces the server declaration with a transport the final agent can open.
4. When that transport connects, sends `_mcp/connect` toward the component that
   declared the identifier.
5. Relays MCP requests and notifications through `_mcp/message`, keyed by the
   returned connection identifier, and sends `_mcp/disconnect` when the bridge
   closes.

The exact underscore-prefixed envelopes are documented in the [Proxy Extension
Protocol Reference](./protocol.md#legacy-mcp-polyfill-methods).

## HTTP Mode

`McpOverAcpPolyfill::http()` is the default compatibility shape. It replaces
the `acp:` declaration with an HTTP MCP URL on `localhost`. The embedded server
accepts MCP POST requests and an SSE GET stream at `/`, retaining JSON-RPC batch
frames and correlating each POST with its response.

```rust,ignore
let bridge = McpOverAcpPolyfill::http();
```

The listener is bound only on loopback and uses an ephemeral port. It does not
implement resumable SSE event IDs.

## Stdio Mode

`McpOverAcpPolyfill::stdio(command)` replaces the declaration with an MCP stdio
server. It appends `mcp PORT` to the supplied command and expects that process
to copy newline-delimited JSON-RPC between stdio and the designated localhost
TCP port.

```rust,ignore
let bridge = McpOverAcpPolyfill::stdio(vec![
    "legacy-mcp-bridge".to_owned(),
]);
```

The current `agent-client-protocol-conductor` binary has no `mcp` subcommand,
so it is not itself a valid stdio bridge command. Use this mode only with a
compatible bridge executable; otherwise use HTTP mode.

## Lifecycle and Failure Behavior

Each accepted bridge connection receives a unique connection ID from
`_mcp/connect`. The polyfill keeps a connection map until the local MCP
transport closes, then removes the entry and sends `_mcp/disconnect`.
Connection or relay failures propagate through the proxy connection rather than
being encoded as notification responses.

The polyfill does not infer or store ACP session IDs. Association is carried by
the MCP server identifier and the resulting MCP connection ID.
