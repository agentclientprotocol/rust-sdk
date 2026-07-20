# Protocol V2

The core SDK can opt into the draft ACP protocol v2 surface with the
`unstable_protocol_v2` crate feature:

```toml
agent-client-protocol = { version = "...", features = ["unstable_protocol_v2"] }
```

This feature is separate from the broad `unstable` feature because protocol v2
is a versioning experiment, not just an unstable method family.

## JSON-RPC batches

The standard `Lines`, `ByteStreams`, and `Stdio` transports accept incoming
JSON-RPC batch arrays. They process each entry independently, preserve valid
entries when a sibling is invalid, and collect all response-bearing entries
into one response array after the batch completes. An empty array receives one
`Invalid Request` response object, while a notification-only batch receives no
response. Incoming response arrays are routed entry by entry using their request
IDs; malformed response-like siblings are ignored rather than answered.

This support lives in the shared transport layer and therefore also works in
v1 mode as a compatibility extension. The SDK does not initiate batches of
requests or notifications. It only writes a batch array when responding to a
batch call received from the peer; requests received individually continue to
receive individual response objects.

ACP peers should still send lifecycle-sensitive calls individually. For
compatibility, the agent protocol router accepts `initialize` as the first
entry of an incoming batch and preserves that batch when handing the connection
to the selected v1 or v2 implementation.

By default, `Client.builder()` and `Agent.builder()` continue to expose the
stable v1 API and advertise protocol v1. To use the v2 API for a connection,
construct the builder with `Client.v2()` or `Agent.v2()`:

```rust
use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Agent, Client};

fn implementation() -> v2::Implementation {
    v2::Implementation::new("example", "0.1.0")
}

# async fn run(agent_transport: impl agent_client_protocol::ConnectTo<agent_client_protocol::Client>) -> agent_client_protocol::Result<()> {
Client
    .v2()
    .connect_with(agent_transport, async |cx| {
        let initialize = cx
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V1,
                implementation(),
            ))
            .block_task()
            .await?;

        assert_eq!(initialize.protocol_version, ProtocolVersion::V2);
        Ok(())
    })
    .await?;
# Ok(())
# }

# async fn serve(client_transport: impl agent_client_protocol::ConnectTo<agent_client_protocol::Agent>) -> agent_client_protocol::Result<()> {
Agent
    .v2()
    .on_receive_request(
        async |initialize: v2::InitializeRequest, responder, _cx| {
            responder.respond(v2::InitializeResponse::new(
                initialize.protocol_version,
                implementation(),
            ))
        },
        agent_client_protocol::on_receive_request!(),
    )
    .connect_to(client_transport)
    .await?;
# Ok(())
# }
```

When v2 mode is enabled, application code should use types from
`agent_client_protocol::schema::v2`. The flat `agent_client_protocol::schema::*`
exports remain the stable v1 schema. This will likely change as v2 gets closer
to release.

The SDK handles the `initialize` negotiation at the JSON-RPC boundary:

- A v2 client advertises protocol v2 as its latest supported version.
- A v2 client requires a v2 agent. If the agent responds with v1, the
  `initialize` request resolves with an error and the caller must explicitly
  fall back to a v1 client implementation if that is acceptable.
- A v2 agent requires a v2 client. If a client initializes with v1, the
  `initialize` request resolves with an error and the caller must use a v1
  agent implementation instead.
- If the agent responds with any other unsupported version, the request resolves
  with an error so the client can close the connection.
- After initialization, the local API version and negotiated wire version must
  match. The SDK does not convert traffic between v1 and v2.

That means v1 and v2 implementations still need separate handlers.
`Agent.v2()` and `Client.v2()` are v2-only. While protocol v2 stabilizes, the
`unstable_protocol_v2` crate feature also exposes `Agent.protocol_router()` and
`Client.protocol_router()` for composing version-specific implementations.

Agents can add protocol implementations independently, which makes it easy for
applications built with v2 support to control v2 rollout with a runtime feature
flag:

```rust
use agent_client_protocol::schema::{v1, v2};
use agent_client_protocol::{Agent, ConnectTo};

# fn implementation() -> v2::Implementation {
#     v2::Implementation::new("example", "0.1.0")
# }
# async fn serve(client_transport: impl agent_client_protocol::ConnectTo<Agent>) -> agent_client_protocol::Result<()> {
# let enable_protocol_v2 = true;
let v1_agent = Agent.builder().on_receive_request(
    async |initialize: v1::InitializeRequest, responder, _cx| {
        responder.respond(v1::InitializeResponse::new(initialize.protocol_version))
    },
    agent_client_protocol::on_receive_request!(),
);

let agent = Agent.protocol_router().with_v1(v1_agent);

let agent = if enable_protocol_v2 {
    let v2_agent = Agent.v2().on_receive_request(
        async |initialize: v2::InitializeRequest, responder, _cx| {
            responder.respond(v2::InitializeResponse::new(
                initialize.protocol_version,
                implementation(),
            ))
        },
        agent_client_protocol::on_receive_request!(),
    );

    agent.with_v2(v2_agent)
} else {
    agent
};

agent
    .connect_to(client_transport)
    .await?;
# Ok(())
# }
```

The protocol router reads the initial `initialize` request, selects the
highest configured protocol version that is compatible with the requested
version, and then hands the connection to that implementation. If only v2 is
configured, v1 clients are rejected without changing the fluent API. The router
does not convert messages between v1 and v2 after routing.

Clients use the same fluent shape:

```rust
use agent_client_protocol::{Client, ConnectTo};

# fn v1_client() -> impl agent_client_protocol::ConnectTo<agent_client_protocol::Agent> {
#     Client.builder()
# }
# fn v2_client() -> impl agent_client_protocol::ConnectTo<agent_client_protocol::Agent> {
#     Client.v2()
# }
# async fn run(agent_transport: impl agent_client_protocol::ConnectTo<agent_client_protocol::Client>) -> agent_client_protocol::Result<()> {
# let enable_protocol_v2 = true;
let client = Client.protocol_router().with_v1(v1_client());

let client = if enable_protocol_v2 {
    client.with_v2(v2_client())
} else {
    client
};

client
    .connect_to(agent_transport)
    .await?;
# Ok(())
# }
```

The client router starts the highest configured implementation. If v2
initialization negotiates v1 and a v1 implementation is configured, it switches
to the local v1 client implementation and forwards the original initialize
response. It does not send a second initialize request on the same connection.
If v2 initialization is rejected, that error is surfaced to the v2 client
implementation.
