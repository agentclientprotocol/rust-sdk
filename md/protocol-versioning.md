# Protocol Versioning

The SDK has an experimental `unstable_protocol_v2` feature for testing ACP v2
before the v2 schema is released. This feature is intentionally separate from
the `unstable` umbrella because enabling draft v2 types must not silently change
stable v1 behavior.

While the schema is unreleased, the workspace depends on the local schema
checkout:

```toml
agent-client-protocol-schema = { path = "../agent-client-protocol", version = "=0.12.2", features = ["tracing"] }
```

## Runtime Version Layer

With `agent-client-protocol/unstable_protocol_v2` enabled:

- `agent_client_protocol::schema::v2` is available from the schema crate.
- `ProtocolVersion::V2` is available, but `ProtocolVersion::LATEST` remains
  `ProtocolVersion::V1`.
- The SDK implements `JsonRpcMessage`, `JsonRpcRequest`,
  `JsonRpcNotification`, and `JsonRpcResponse` for the v2 request,
  notification, and response types.
- Connections track the protocol version negotiated by `initialize`.
- The v2 impls serialize as native v2 JSON when the connection has negotiated
  v2, and convert through `schema::v2::conversion` when the peer negotiated v1.

SDK authors can write handlers and outbound calls in terms of v2 types while
the connection decides whether the peer should see v1-compatible or native v2
payloads.

For example, an agent can register a v2 initialize handler:

```rust
use agent_client_protocol::{Agent, ConnectionTo, Responder};
use agent_client_protocol::schema::{ProtocolVersion, v2};

Agent.builder()
    .on_receive_request(
        async |request: v2::InitializeRequest,
               responder: Responder<v2::InitializeResponse>,
               _cx: ConnectionTo<agent_client_protocol::Client>| {
            let protocol_version = if request.protocol_version >= ProtocolVersion::V2 {
                ProtocolVersion::V2
            } else {
                ProtocolVersion::V1
            };

            responder.respond(v2::InitializeResponse::new(protocol_version))
        },
        agent_client_protocol::on_receive_request!(),
    );
```

A v1 client can still initialize that agent because the SDK parses incoming v1
initialize parameters, converts them to v2 before invoking the handler, and
converts the v2 response back to a v1-compatible response payload.

The same low-level typed message path works on the client side:

```rust
let response = connection
    .send_request(v2::InitializeRequest::new(ProtocolVersion::V2))
    .block_task()
    .await?;
```

If the agent responds with `ProtocolVersion::V1`, the client can keep using v2
Rust types while the SDK compatibility layer preserves v1 wire payloads.

## Negotiation Flow

1. Connections start without a negotiated ACP version.
2. The client sends `InitializeRequest { protocol_version: V2, ... }` when it
   wants v2.
3. The agent responds with the protocol version it selected.
4. `ConnectionTo`, `Responder`, and `SentRequest` record that selected version.
5. Later requests, notifications, and responses transcode between the author's
   Rust API version and the negotiated peer wire version.

The negotiated version is visible through
`ConnectionTo::negotiated_protocol_version()`. It is `None` before initialize
completes.

## Current Limits

- The runtime codec is wired up, but the current draft v2 schema still mostly
  mirrors v1, so there are not many observable wire-shape differences yet.
- The direct typed message APIs work for both agent and client roles, but
  higher-level helpers such as `ConnectionTo::build_session` still construct v1
  request and response types.
- Proxies and conductor flows still forward untyped messages as they do today.
  Proxy boundaries still need an explicit policy for whether they preserve,
  upgrade, or downgrade payloads between each peer.
