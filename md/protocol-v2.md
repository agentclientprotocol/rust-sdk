# Protocol V2

The core SDK can opt into the draft ACP protocol v2 surface with the
`unstable_protocol_v2` crate feature:

```toml
agent-client-protocol = { version = "...", features = ["unstable_protocol_v2"] }
```

This feature is separate from the broad `unstable` feature because protocol v2
is a versioning experiment, not just an unstable method family.

## JSON-RPC batches

Batch framing is a shared JSON-RPC transport feature, not a v2-only protocol
feature. Both v1 and v2 accept incoming batches, preserve them through relays,
and group replies into one response array. The SDK does not originate batches
of requests or notifications. See [Transport Architecture: JSON-RPC Batch
Behavior](./transport-architecture.md#json-rpc-batch-behavior) for the complete
rules.

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
                ProtocolVersion::V2,
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
`Client.protocol_connector()` for composing version-specific implementations.

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
does not convert messages between v1 and v2 after routing. For compatibility,
the initial frame may be a batch whose first call-shaped entry is `initialize`;
the router preserves the complete frame when handing it to the selected
implementation. Response-only frames before initialization are ignored.

Clients use a connector because fallback may require opening a new transport.
Both client implementations and the agent transport are factories:

```rust,ignore
use agent_client_protocol::Client;

let connector = Client
    .protocol_connector()
    .with_v1(|| v1_client())
    .with_v2(|| v2_client());

connector.connect_to(|| open_agent_transport()).await?;
```

The connector starts the highest configured implementation. If a successful v2
initialize response negotiates v1 and a v1 implementation is configured, the
connector starts the v1 implementation and compares the initialize metadata and
capabilities it would send with the normalized v2 request already seen by the
agent:

- If they match exactly, the connector reuses the current agent connection and
  delivers the original response to the v1 implementation with its request ID.
  It does not send a second initialize request.
- If they differ, the connector closes that connection, calls both factories
  again as needed, and performs a fresh v1 initialization on a new agent
  connection.
- If the agent rejects the v2 initialize request, the error is surfaced. A
  rejected initialize is not treated as permission to retry with v1.

## Draft schema changes in schema 1.5

The `unstable_protocol_v2` API follows the moving draft schema. Schema 1.5 adds
semantic newtypes for paths, media types, IDs, and cursors; renames
`DiffPatch.diff` to `DiffPatch.text`; adds terminal state and output update
types; and makes v1/v2 conversions fallible and generic. These are draft API
changes rather than stable v1 wire changes. See [Migrating to
v2.0](./migration_v2.0.md#draft-v2-schema-updates) for concrete source changes.
