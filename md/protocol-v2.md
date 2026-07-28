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
construct the builder with `Client.v2()` or `Agent.v2()`. Fluent typed handlers,
spawned tasks, close callbacks, and `connect_with` receive
`V2ConnectionTo<_>`, so the protocol version is reflected in the high-level
Rust API as well as on the wire:

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

## High-level v2 sessions

Stable callbacks receive `ConnectionTo<_>` and expose the protocol v1
`build_session*`, `SessionBuilder`, `ActiveSession`, and `SessionMessage` APIs.
Callbacks installed through `Client.v2()` receive `V2ConnectionTo<_>` and expose
the v2 `build_session*` and `resume_session*` helpers. The shared names describe
the same lifecycle operations while the connection type selects their schema
and return types at compile time.

Low-level custom `with_handler` and `with_runner` implementations continue to
receive the protocol-neutral `ConnectionTo<_>`, and generic `send_request`
remains schema-agnostic. Runtime compatibility checks remain at those explicit
escape hatches. Dynamic handlers registered through
`V2ConnectionTo::add_dynamic_handler` use the same low-level
`HandleDispatchFrom` interface and therefore also receive `ConnectionTo<_>`.

Nested connections preserve the stable `ConnectionTo` API while still typing
the child implementation's callbacks. On a raw `ConnectionTo<_>`,
`spawn_connection(Client.v2(), transport)` returns a raw `ConnectionTo<_>`;
callbacks installed on that v2 child builder still receive
`V2ConnectionTo<_>`. Existing `spawn_connection::<Role>` calls therefore remain
source-compatible.

When a raw parent also needs a typed handle to the v2 child, the
`unstable_protocol_v2` feature exposes
`ConnectionTo::spawn_connection_with_context`, which returns the context
selected by the child builder. `V2ConnectionTo::spawn_connection` likewise
follows the child builder naturally, so spawning `Client.v2()` through an
already-typed v2 connection returns another `V2ConnectionTo<_>`.

```rust,ignore
use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Client, Responder};

Client
    .v2()
    .on_receive_notification(
        async |update: v2::UpdateSessionNotification, _cx| {
            apply_session_update(update)?;
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    )
    .on_receive_request(
        async |request: v2::RequestPermissionRequest,
               responder: Responder<v2::RequestPermissionResponse>,
               _cx| {
            // Transfer the responder to application-owned permission handling
            // without waiting for user input in the dispatch callback.
            queue_permission_request(request, responder)?;
            Ok(())
        },
        agent_client_protocol::on_receive_request!(),
    )
    .connect_with(agent_transport, async |cx| {
        let initialize = cx
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("example", "0.1.0"),
            ))
            .block_task()
            .await?;
        assert!(initialize.capabilities.session.is_some());

        let opened = cx
            .build_session_cwd()?
            .start_session()
            .block_task()
            .await?;
        let (session, new_session_response) = opened.into_parts();
        assert_eq!(session.session_id(), &new_session_response.session_id);

        session
            .send_prompt("What is 2 + 2?")
            .block_task()
            .await?;
        println!("prompt accepted");

        Ok(())
    })
    .await?;
```

Here `apply_session_update` updates application-owned state, while
`queue_permission_request` transfers the request and its responder to a
separate permission workflow.

V2 deliberately separates prompt submission from session observation:

- `session/prompt` returns a `PromptResponse` as soon as the agent accepts the
  prompt. `V2Session::send_prompt` returns that request as a
  `SentRequest<PromptResponse>`; callers must explicitly await it, register a
  response callback, or detach it.
- `V2SessionBuilder::start_session` likewise returns a mapped `SentRequest`.
  Its `OpenedV2Session` result keeps the command handle separate from the
  complete `NewSessionResponse` represented by the linked schema, rather than
  reconstructing a selected subset of its fields.
- `V2Session` is a cloneable command handle containing only the session ID and
  connection. It does not own, buffer, or unregister inbound messages.
- Register typed `UpdateSessionNotification` and `RequestPermissionRequest`
  handlers on `Client.v2()` before connecting. Updates and interactive requests
  are separate protocol lanes; permission handlers should transfer responders
  to application-owned work rather than waiting for user input inside the
  connection dispatch loop. Without matching handlers, unhandled v2
  notifications are ignored and unhandled requests receive a method-not-found
  response; they are not retained for a later per-session receiver.
- `session/update` events can arrive before, during, or after a prompt request.
  They carry a session ID and entity IDs, but no prompt or turn ID. The SDK
  therefore does not attribute intervening events to a locally submitted
  prompt or provide a prompt-scoped text accumulator.
- `state_update` describes the session-wide foreground state. `idle` means the
  session can accept ordinary new foreground work; it is not a wire-level
  boundary assigning previous events to one prompt, and background updates may
  continue while idle.
- `cancel_active_work` sends session-wide `session/cancel`. Cancellation
  completes after the required `idle` update with stop reason `cancelled`. The
  client should immediately mark unfinished tool calls for the active work as
  cancelled and must resolve every pending permission request with the
  cancelled outcome. Cancelling or dropping the prompt's `SentRequest` is the
  separate JSON-RPC request-cancellation mechanism.
- `set_config_option` returns the authoritative replacement option set, and
  `close` returns the complete close response. Mutable configuration is not
  cached on the command handle.

Install connection handlers before `session/new` and `session/resume` requests.
This is especially important for `resume_session_from`: replay updates
precede the resume response on the wire, so preinstalled typed handlers observe
them in order. If a handler forwards updates to another task, the application
is responsible for any additional projection-drained barrier it needs before
treating replay as locally applied.

Dropping command handles has no network or inbound-routing side effect. For a
session created with `V2SessionBuilder::with_mcp_server`, the SDK installs the
MCP routes and initially polls their runner tasks before publishing
`session/new`, so the agent can connect to those servers during session setup.
Runners may continue asynchronous initialization; custom connectors must be
able to queue connections and messages once constructed. A successful setup
promotes the attachment to the connection lifetime; any setup failure cleans it
up. This attachment requires both `unstable_protocol_v2` and
`unstable_mcp_over_acp`. Global proxy attachment and proxy-session helpers
remain v1-only.

If an application wants stream ergonomics, it can fan typed updates out from
the connection handler with an explicit buffering and subscriber policy.

## Conductor and proxy initialization

Enable `unstable_protocol_v2` on `agent-client-protocol-conductor` to carry a v2
connection through a conductor proxy chain. The conductor inspects the raw
`protocolVersion` before parsing initialization, rewrites ordinary `initialize`
to `_proxy/initialize` without reserializing its parameters, and restores the
ordinary method before the request reaches the final agent. For an exact v2
request, `info`, `capabilities`, metadata, and unknown extension fields
therefore retain their wire shape across conductor-controlled rewrites. A proxy
implementation can still deliberately replace the request it forwards.

As with the core protocol router, an exact v2 request can retain unknown raw
fields, while a request for a later compatible version is canonicalized through
the selected v2 schema before component instantiation.

Proxy implementations use
`agent_client_protocol::schema::v2::InitializeProxyRequest`; its response is the
v2 `InitializeResponse`. The flat `schema::InitializeProxyRequest` remains the
stable v1 type. Static conductor component providers support both versions.
Custom `InstantiateProxiesAndAgent` and `InstantiateProxies` implementations
opt into v2 by implementing their feature-gated v2 method; the default rejects
v2 rather than interpreting it as v1. Returning the initialize request
unchanged preserves its complete raw parameters for an exact-version request,
including unknown extensions; returning a modified typed request makes that
serialized request authoritative. The conductor pins `protocolVersion` to its
selected implementation even if an instantiator attempts to change it, and
validates the final agent's initialize response against that selection.
The proxy connection also routes v2 `session/new` requests and responses
without interpreting them as v1 payloads.

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
connector starts the v1 implementation and compares the complete initialize
parameters it would send with the normalized v2 request already seen by the
agent:

- If they match exactly, the connector reuses the current agent connection and
  delivers the original response to the v1 implementation with its request ID.
  It does not send a second initialize request.
- If they differ, the connector closes that connection, calls both factories
  again as needed, and performs a fresh v1 initialization on a new agent
  connection.
- If the agent rejects the v2 initialize request, the error is surfaced. A
  rejected initialize is not treated as permission to retry with v1.

## Draft schema changes in schema 1.5 and 1.6

The `unstable_protocol_v2` API follows the moving draft schema. Schema 1.5 added
semantic newtypes for paths, media types, IDs, and cursors; renamed
`DiffPatch.diff` to `DiffPatch.text`; and added terminal state and output update
types. The next schema dependency update removes the former schema-wide v1/v2
conversion API: versioned implementations should remain separate, with
purpose-specific adapters at runtime boundaries where the required state and
policy are available. These are draft API changes rather than stable v1 wire
changes. See [Migrating to
v2.0](./migration_v2.0.md#draft-v2-schema-updates) for concrete source changes.

Schema 1.6 adds `Cancelled` tool-call and plan-entry statuses to draft v2.
Programmatic tool-call names are available in both protocol versions through
the separate `unstable_tool_call_name` feature. Draft v2 users must enable both
`unstable_protocol_v2` and `unstable_tool_call_name`. In v2, an omitted name
leaves the existing value unchanged, `null` clears it, and a string replaces
it. V1 cannot express the explicit v2 `null` clear operation.
