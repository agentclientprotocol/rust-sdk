# Protocol V2

The core SDK can opt into the draft ACP protocol v2 surface with the
`unstable_protocol_v2` crate feature:

```toml
agent-client-protocol = { version = "...", features = ["unstable_protocol_v2"] }
```

This feature is separate from the broad `unstable` feature because protocol v2
is a versioning experiment, not just an unstable method family.

By default, `Client.builder()` and `Agent.builder()` continue to expose the
stable v1 API and advertise protocol v1. To use the v2 API for a connection,
construct the builder with `Client.v2()` or `Agent.v2()`:

```rust
use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Agent, Client};

# async fn run(agent_transport: impl agent_client_protocol::ConnectTo<agent_client_protocol::Client>) -> agent_client_protocol::Result<()> {
Client
    .v2()
    .connect_with(agent_transport, async |cx| {
        let initialize = cx
            .send_request(v2::InitializeRequest::new(ProtocolVersion::V1))
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
            responder.respond(v2::InitializeResponse::new(initialize.protocol_version))
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
- A v2 agent responds with v2 when the client supports it, or v1 when the client
  only supports v1. Agent handlers still receive v2 schema types; the SDK tracks
  the negotiated wire version separately and adapts supported behavior at the
  transport boundary.
- If the agent responds with any other unsupported version, the request resolves
  with an error so the client can close the connection.
- After initialization, the SDK converts supported messages and responses between
  the local API version and the negotiated wire version.

That means an agent can be implemented against v2 request and response types
while still serving v1 clients. The goal is for agent-side v1 compatibility to
live in the SDK wherever it can be represented as protocol adaptation. Clients
should opt into v2 separately and should not assume v2 behavior from v1 agents.
