# Protocol V2

The core SDK can opt into the draft ACP protocol v2 surface with the
`unstable_protocol_v2` crate feature:

```toml
agent-client-protocol = { version = "...", features = ["unstable_protocol_v2"] }
```

This feature is separate from the broad `unstable` feature because protocol v2
is a versioning experiment, not just an unstable method family.

To start from working code, build and run the companion agent and client in the
[Runnable Protocol V2 Quickstart](./protocol-v2-quickstart.md). The examples
exercise prompt acceptance, independent session updates, and the terminal idle
state over a real stdio connection.

## JSON-RPC batches

Protocol v2 removed JSON-RPC batching, and the SDK no longer supports it in
either protocol version. Every connection answers an incoming JSON array with a
single `-32600` (`Invalid Request`) error with `id: null` and processes none of
its entries. The SDK never sends a batch. See [Transport Architecture: JSON-RPC
Batches](./transport-architecture.md#json-rpc-batches).

Every connection names its version: `Client.v1()`, `Agent.v1()`, and
`Proxy.v1()` expose the stable v1 API, and `Client.v2()`, `Agent.v2()`, and
`Proxy.v2()` the v2 API. The SDK types for each version live in
`agent_client_protocol::v1` and `agent_client_protocol::v2`, which export the
same names: `ConnectionTo`, `Builder`, `Responder`, `ResponseRouter`,
`SentRequest`, `PreparedRequest`, `Dispatch`, `SessionBuilder`,
`ResumeSessionBuilder`, `SessionSetup`, `ActiveSession`, and `schema` (plus
`ForkSessionBuilder` and the `send_initialize` outcome types in `v2`). Moving code from v1 to v2 is
mostly a matter of changing which module it imports from.
Fluent typed handlers, spawned tasks, close callbacks, and `connect_with`
receive `v2::ConnectionTo<_>`, so the protocol version is reflected in the
high-level Rust API as well as on the wire:

```rust
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v2::schema;
use agent_client_protocol::{Agent, Client};

fn implementation() -> schema::Implementation {
    schema::Implementation::new("example", "0.1.0")
}

# async fn run(agent_transport: impl agent_client_protocol::ConnectTo<agent_client_protocol::Client>) -> agent_client_protocol::Result<()> {
Client
    .v2()
    .connect_with(agent_transport, async |cx| {
        let initialize = cx
            .send_request(schema::InitializeRequest::new(
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
        async |initialize: schema::InitializeRequest, responder, _cx| {
            responder.respond(schema::InitializeResponse::new(
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

When v2 mode is enabled, application code should use SDK types from
`agent_client_protocol::v2` and message types from `v2::schema`, the same
module as `agent_client_protocol::schema::v2`. Importing
`agent_client_protocol::v2::{self, schema}` brings both into scope without a
name clash. The preceding agent fragment demonstrates version negotiation only.
An agent that advertises session support must also implement the complete
baseline session surface shown in the runnable quickstart.

## High-level v2 sessions

Stable callbacks receive `v1::ConnectionTo<_>` and expose the protocol v1
`build_session*`, `load_session*`, `resume_session*`,
`v1::SessionBuilder`, `v1::ActiveSession`, and `v1::SessionMessage` APIs.
Callbacks installed through `Client.v2()` receive `v2::ConnectionTo<_>` and expose
the v2 `build_session*` and `resume_session*` helpers, plus feature-gated
`fork_session*` helpers when `unstable_session_fork` is enabled. The shared
names describe the same lifecycle operations while the connection type selects
their schema and return types at compile time. Resume and fork return
`v2::ResumeSessionBuilder` and `v2::ForkSessionBuilder`. Call `start_session` to
publish the request and obtain a `v2::SessionSetup` containing the command
handle and complete operation-specific response.

Low-level custom `with_handler` and `with_runner` implementations continue to
receive the protocol-neutral `ConnectionTo<_>`, and generic `send_request`
remains schema-agnostic. These generic APIs do not infer a protocol version from
the Rust payload type: callers on a v2 connection must use `schema::v2` types,
or deliberately send extension or untyped messages. The connection guard
enforces negotiation and initialization lifecycle, not Rust-type provenance.
Dynamic handlers registered through `v2::ConnectionTo::add_dynamic_handler` use
the same low-level `HandleDispatchFrom` interface and therefore also receive
`ConnectionTo<_>`. Raw code converts explicitly: `connection.into_protocol::<V1>()`
returns a `v1::ConnectionTo` (and `into_protocol::<V2>()` a `v2::ConnectionTo`),
and either versioned connection's `into_raw()` returns the version-neutral one.

Nested connections preserve the stable `ConnectionTo` API while still typing
the child implementation's callbacks. On a raw `ConnectionTo<_>`,
`spawn_connection(Client.v2(), transport)` returns a raw `ConnectionTo<_>`, and
callbacks installed on that v2 child builder still receive
`v2::ConnectionTo<_>`. Existing `spawn_connection::<Role>` calls therefore remain
source-compatible.

When a raw parent also needs a typed handle to the child,
`ConnectionTo::spawn_connection_with_context` returns the context selected by
the child builder. `v1::ConnectionTo::spawn_connection` and
`v2::ConnectionTo::spawn_connection` likewise follow the child builder
naturally, so spawning `Client.v2()` through an already-typed v2 connection
returns another `v2::ConnectionTo<_>`, and spawning `Client.v1()` through any
typed connection returns a `v1::ConnectionTo<_>`.

A complete client installs update and interactive-request handlers before
connecting. After `session/prompt` is accepted, it must keep the connection
alive and consume updates until the matching session reaches idle. The
[`v2_one_shot_client`](https://github.com/agentclientprotocol/rust-sdk/blob/main/src/agent-client-protocol/examples/v2_one_shot_client.rs)
example demonstrates the full sequence, while the compiled cookbook
`v2_one_shot_prompt` recipe shows how to embed it in an application. Permission
handlers should transfer the request and responder to application-owned work
rather than waiting for user input inside the dispatch callback.

V2 deliberately separates prompt submission from session observation:

- `session/prompt` returns a `PromptResponse` as soon as the agent accepts the
  prompt. Its `message_id` identifies the inserted user message, and live
  `UserMessage` updates for that prompt use the same ID. Persisting the message
  in resumable history remains optional. The response does not indicate turn
  completion. `v2::ActiveSession::send_prompt` returns that request as a
  `SentRequest<PromptResponse, V2>`. Callers must explicitly await it, register a
  response callback, or detach it.
- `v2::SessionBuilder::start_session` likewise returns a mapped `SentRequest`.
  Its `v2::SessionSetup` result keeps the command handle separate from the
  complete `NewSessionResponse` represented by the linked schema, rather than
  reconstructing a selected subset of its fields.
- `v2::ConnectionTo::resume_session` and `resume_session_from` return a
  `v2::ResumeSessionBuilder`. Its `start_session` method publishes
  `session/resume` and returns a `v2::SessionSetup` containing the complete
  `ResumeSessionResponse` without reconstructing it.
- With `unstable_session_fork`, `v2::ConnectionTo::fork_session` and
  `fork_session_from` return a `v2::ForkSessionBuilder`. Its `start_session`
  publishes `session/fork`, preserves the complete `ForkSessionResponse`, and
  uses that response's newly allocated session ID for the command handle.
- `v2::ActiveSession` is a cloneable command handle containing only the session ID and
  connection. It does not own, buffer, or unregister inbound messages.
- Register typed `UpdateSessionNotification` and `RequestPermissionRequest`
  handlers on `Client.v2()` before connecting. Updates and interactive requests
  are separate protocol lanes. Permission handlers should transfer responders
  to application-owned work rather than waiting for user input inside the
  connection dispatch loop. Without matching handlers, unhandled v2
  notifications are ignored and unhandled requests receive a method-not-found
  response. They are not retained for a later per-session receiver.
- `session/update` events can arrive before, during, or after a prompt request.
  They carry a session ID and entity IDs, but no prompt or turn ID. The SDK
  therefore does not attribute intervening events to a locally submitted
  prompt or provide a prompt-scoped text accumulator.
- `state_update` describes the session-wide foreground state. `idle` means the
  session can accept ordinary new foreground work. It is not a wire-level
  boundary assigning previous events to one prompt, and background updates may
  continue while idle.
- `send_prompt` may be called in any state. The agent inserts the prompt right
  away or holds it and inserts held prompts in received order; it must not
  reject a prompt only because foreground work is active. A held prompt's
  response stays pending until insertion, and cancelling its `SentRequest`
  withdraws it with `-32800`.
- `cancel_active_work` sends session-wide `session/cancel`. It covers the
  session's foreground work and prompts received before it that the agent has
  not yet inserted; those prompt requests fail with `-32800` (request
  cancelled), while a prompt whose user message was already inserted still
  succeeds with its message ID. A prompt sent after it is not cancelled.
  Background activity is not covered, although the agent may stop it too.
  Cancellation of active foreground work completes after the required `idle`
  update with stop reason `cancelled`; cancelling an idle session need not
  emit another completion. `cancelled` is reserved for work the client
  cancelled with `session/cancel` or by closing the session; work the agent
  stops on its own uses another reason, such as `error`. The client should immediately mark unfinished tool calls for the active work as
  cancelled and must resolve every pending permission request in that session
  with the cancelled outcome, without answering twice or affecting other
  sessions. Elicitations are not all cancelled implicitly: agents should use
  individual `$/cancel_request` for interactions made irrelevant by the
  cancelled work. Cancelling or dropping the prompt's `SentRequest` is the
  separate JSON-RPC request-cancellation mechanism; insertion still requires a
  successful prompt response with its message ID.
- `set_config_option` must only name a `configId` the agent advertised for
  the session. An unknown `configId` or invalid value fails with `-32602` and
  leaves the configuration unchanged. A successful response is the complete
  option set as of the response and may report a different `currentValue`
  than requested. Apply responses and `config_option_update` notifications in
  the order received, each replacing the whole list, even though a duplicate
  notification for the same change may arrive before the response. `close` returns the complete close response. Closing is
  idempotent, so `close` also resolves a `-32002` response (an unknown or
  already-closed session) as success. Mutable configuration is not cached on
  the command handle.

Install connection handlers before `session/new`, `session/resume`, and
feature-gated `session/fork` requests. This is especially important before
calling `start_session` on a `v2::ResumeSessionBuilder`: replay updates precede
the resume response on the wire, so preinstalled typed handlers observe them in
order. If a handler forwards updates to another task, the application is
responsible for any additional projection-drained barrier it needs before
treating replay as locally applied.
See [Ordered Application Dispatch](./ordered-application-dispatch.md) for a
single-consumer queue that carries updates, response results, and closure.
For an application-owned prototype that additionally coordinates shared resume
replay, abandonment, and close, see
[Session Operation Coordination](./session-operation-coordination.md). It is a
cookbook policy example, not a public SDK coordinator.

Dropping command handles has no network or inbound-routing side effect. For a
session configured with `v2::SessionBuilder::with_mcp_server` or
`v2::ResumeSessionBuilder::with_mcp_server`, or feature-gated
`v2::ForkSessionBuilder::with_mcp_server`, the SDK installs the MCP routes and
initially polls their runner tasks before publishing the corresponding setup
request, so the agent can connect to those servers during setup or resume
replay. Runners may continue asynchronous initialization, so custom connectors
must be able to queue connections and messages once constructed. A successful
setup promotes the attachment to the connection lifetime. A setup failure,
including an error response after cancellation, cleans up the pending
attachment. This attachment requires both `unstable_protocol_v2` and
`unstable_mcp_over_acp`. Fork additionally requires
`unstable_session_fork`.

A v2 proxy can instead attach one server globally with
`Proxy.v2().with_mcp_server(...)`. The proxy reuses one connection-scoped
server ID and adds its declaration to v2 `session/new`, `session/resume`, and
feature-gated `session/fork` requests. It modifies only the `mcpServers` field,
preserving unrelated setup fields and extensions for downstream handlers.
MCP server names must be unique within `mcpServers`. If the client already
declares a server with the proxy server's name, the proxy fails that request
with `-32602`, naming the server it configures, instead of forwarding a request
the agent must reject. It does not rename or drop either server.

`v2::SessionBuilder::on_proxy_session_start` and
`v2::ResumeSessionBuilder::on_proxy_session_start`, plus
`v2::ForkSessionBuilder::on_proxy_session_start` when enabled, are the
non-blocking setup helpers for a v2 proxy:

```rust,ignore
use agent_client_protocol::schema::v2;
use agent_client_protocol::v2::SessionSetup;
use agent_client_protocol::{Client, Proxy};

Proxy
    .v2()
    .on_receive_request_from(
        Client,
        async |request: v2::NewSessionRequest, responder, cx| {
            cx.build_session_from(request)
                .with_mcp_server(session_server)?
                .on_proxy_session_start(responder, async |setup| {
                    let SessionSetup { session, response, .. } = setup;
                    record_session(session.session_id(), response);
                    Ok(())
                })
        },
        agent_client_protocol::on_receive_request!(),
    );
```

These helpers forward request cancellation, send an ordered downstream setup
request, and forward the complete operation-specific response without
reconstruction. For `session/new` and `session/fork`, routing is installed when
the response makes the new session ID available and before later inbound
traffic is dispatched. Fork routing uses the response's new ID rather than the
source session ID. For `session/resume`, the ID is already known, so routing and any per-session
MCP attachment are ready before the downstream request is published. Replay
updates can therefore be forwarded upstream before the complete
`ResumeSessionResponse`, as required by the protocol. A failed or cancelled
downstream response drops pending routing and MCP attachment. Successful setup
keeps them for the connection lifetime. A cancellation signal itself remains
advisory: it is forwarded downstream while the helper awaits that response.
Each helper then spawns the callback outside the ordering barrier with an
`v2::SessionSetup` containing the command-only session handle and complete setup
response. Updates and interactive requests remain independent connection
traffic and should still be handled by typed callbacks on `Proxy.v2()`.

If an application wants stream ergonomics, it can fan typed updates out from
the connection handler with an explicit buffering and subscriber policy.

## Conductor and proxy initialization

Proxy authors should make the version boundary explicit. `Proxy.v1()` is
the stable v1 builder, while `Proxy.v2()` is v2-only. A proxy negotiates
`proxy/initialize` exactly as an agent of the same version negotiates
`initialize`: a v1 proxy selects v1 for every request, keeping only what
parses under v1 for a newer one; a v2 proxy selects v2 for a request for v2 or
newer, keeping only what parses under v2 for a newer one, and rejects a
request below v2 with `-32022`. A proxy never parses one version's request
through a permissive schema for another.

When one component must expose independently authored v1 and v2 proxies,
compose them with
`Proxy.protocol_router().with_v1(v1_proxy).with_v2(v2_proxy)`. The proxy
router selects an implementation from `proxy/initialize` exactly as
`Agent.protocol_router()` does from `initialize`: a request for v2 or newer
selects v2, and with a v1 implementation configured everything else goes to
v1, which negotiates it as it would alone. It hands over the complete,
unchanged initial transport frame for a request for exactly the selected
version and does not convert later traffic.

Components implementing their own raw version selector can use
`Builder::new(Proxy)` and keep protocol-neutral `ConnectionTo` callbacks.
`Builder::new` applies no protocol-version guard, so it is not a substitute
for selecting `Proxy.v2()` in an ordinary v2 proxy implementation. A proxy's default forwarding is the same for every builder and
every protocol version: `proxy/initialize` has the same wire shape in v1 and
v2, so the proxy forwards untyped messages, and fields the SDK does not know
survive. After a forwarded `session/new` succeeds, the proxy reads the raw
`sessionId` from the response and routes that session's updates back to the
client.

The `unstable_protocol_v2` feature only adds v2 items. Enabling it never
changes how a v1 connection behaves: where one code path serves both versions,
the SDK selects v2 behavior from the connection's protocol version at runtime.

The conductor forwards both versions through one untyped path that does not
depend on the feature or on the selected version. Its connections to the
client and to the final agent have no version guard; the endpoints validate
the traffic of the version the chain negotiated. The conductor rejects a
`proxy/initialize` that arrives from outside, renames the client's
`initialize` to `proxy/initialize` for the first proxy without touching its
params (the proxy restores the ordinary method before the request reaches the
final agent), and forwards everything else unchanged. `info`, `capabilities`,
metadata, and unknown extension fields therefore retain their wire shape
across conductor-controlled rewrites in both versions. A proxy implementation
can still deliberately replace the request it forwards.

Before it instantiates the chain, the conductor selects the version from the
raw `protocolVersion`: with the conductor crate's `unstable_protocol_v2`
feature, a request for v2 or newer selects v2; every other request selects v1,
keeping v1's fallback-to-latest rule. A request for a newer version than the
selected one keeps only what parses under the selected version, as the SDK's
own agents negotiate (`agent_client_protocol::util::initialize_params_for_version`).
A conductor acting as a proxy negotiates its `proxy/initialize` the same way.

The conductor's selection is the highest version the chain may speak, and it
forwards the request at that version. Because each proxy negotiates like an
agent, a proxy that supports only a lower version initializes the rest of the
chain with that version, and the `initialize` response that reaches the client
carries the version every component accepted. A component that cannot accept
it, such as a v2-only agent behind a v1-only proxy, rejects the
initialization, and the client sees the error.

Proxy implementations use
`agent_client_protocol::schema::v2::InitializeProxyRequest`. Its response is the
v2 `InitializeResponse`. `schema::v1::InitializeProxyRequest` is the v1
type. Static conductor component providers can carry either selected
schema, but each supplied component must support that version. Use
`Agent.protocol_router()` or `Proxy.protocol_router()` when a static component
has separate implementations.
Custom `InstantiateProxiesAndAgent` and `InstantiateProxies` implementations
receive a `SelectedInitializeRequest` whose variant is the selected version.
An instantiator that supports only a lower version negotiates down by
returning the request for that version (for example with
`SelectedInitializeRequest::into_v1`, which keeps what parses under v1).
Returning the initialize request unchanged preserves its complete raw
parameters for an exact-version request, including unknown extensions.
Returning a modified typed request makes that serialized request
authoritative. The conductor sets `protocolVersion` to the version of the
returned variant even if an instantiator attempts to change the field, and
rejects a variant above the selected version.

### MCP compatibility polyfill

The concrete
`agent_client_protocol_polyfill::mcp_over_acp::McpOverAcpPolyfill` can
participate in a v2 conductor chain when its `unstable_protocol_v2` feature is
enabled. It selects v1 or v2 from `proxy/initialize`, uses that version's MCP
capability and wire types, and adapts native `McpServer::Acp` declarations in
v2 `session/new`, `session/resume`, and `session/fork` requests.
Other declarations and unrelated request fields remain unchanged. See
[MCP-over-ACP Compatibility Bridge](./mcp-bridge.md) for placement and feature
configuration.

This feature extends the concrete compatibility proxy only. The core SDK's
global MCP attachment and proxy-session helpers support v1 and v2 independently,
as described above.

## Version routing

The SDK handles the `initialize` negotiation at the JSON-RPC boundary:

- Native `Client.v2()` and `Agent.v2()` connections reject ordinary protocol
  traffic until the initialization response completes. `$/cancel_request`
  remains available while initialization is in progress. The client is the
  initializer and the agent is the responder. Attempts in the opposite
  direction are rejected. An initialization error leaves the connection
  uninitialized so the client can retry. A second `initialize` while one is in
  flight is rejected with `-32600`.
- The client may repeat `initialize` after the previous one completed and
  before it sends any other request, for example to resend its parameters for
  the version the agent selected. The agent dispatches each repeat to its
  initialize handler and the latest one is authoritative. While a repeat is in
  flight, other requests are rejected as they are during the first. A repeat
  requesting a version below the selected one is rejected with `-32600`. A
  repeat that fails, whether rejected by the SDK or by the handler, keeps the
  previous initialization in place and the client may repeat again. The first
  other request closes this window: a later `initialize` is rejected with
  `-32600` on both sides and the connection stays ready. Notifications such as
  `$/cancel_request` do not close it.
- V2 request ids must be strings or integers. A v2 connection answers a request
  with a `null` id with `-32600` and `id: null`. V1 connections still accept
  `null` ids.
- Since schema 1.13, v2 has its own `schema::v2::RequestId` without a `Null`
  variant. Request handles carry a protocol marker as their last type
  parameter: `Responder<T, V2>`, `ResponseRouter<T, V2>`,
  `SentRequest<T, V2>`, `PreparedRequest<T, V2>` and
  `Dispatch<Req, Notif, V2>`, named `v2::Responder<T>`,
  `v2::ResponseRouter<T>`, `v2::SentRequest<T>`, `v2::PreparedRequest<T>` and
  `v2::Dispatch<Req, Notif>`. Handles from `Client.v2()`, `Agent.v2()`,
  `Proxy.v2()` and `v2::ConnectionTo` use `V2`, so their `id()` returns a
  `schema::v2::RequestId`, for example to match an elicitation's `requestId`.
  It never fails, because the SDK rejects `null` IDs on v2 connections and
  never assigns `null` itself. v1 builders use `V1`. Low-level
  `HandleDispatchFrom` and `MatchDispatch` handlers are not tied to a version:
  they receive handles with the default `Raw` marker, whose `id()` is the
  version-neutral `RawJsonRpcId`. Convert one to a v2 handle with
  `into_protocol::<V2>()`, for example to call `on_proxy_session_start` from a
  raw router. Every handle also has `raw_id()`.
- Closures registered on a v2 builder that spell out the responder type need
  the v2 handle: `responder: v2::Responder<schema::PromptResponse>` with
  `use agent_client_protocol::v2::{self, schema}` (or
  `Responder<schema::PromptResponse, V2>`). Unannotated
  closure parameters are inferred.
- A v2 client advertises protocol v2 as its latest supported version.
- A v2 client requires a v2 agent. Send initialization with
  `send_initialize` to learn how the agent negotiated: it resolves with
  `v2::InitializeOutcome::Initialized` for a v2 selection, or
  `v2::InitializeOutcome::LowerVersion` with the agent's validated response for
  the selected version (`v2::SelectedInitializeResponse::V1`). After a lower
  selection the connection stays uninitialized, and the caller can explicitly
  fall back to a v1 client implementation if that is acceptable. Errors
  returned by the agent, including `-32022`, malformed responses, and transport
  failures still resolve with errors. A plain `send_request` of the initialize
  request instead resolves a v1 selection with a local error.
- A native v1 agent selects v1 for requests advertising v1 or a newer version,
  with or without `unstable_protocol_v2` enabled. Newer initialization metadata
  is normalized into v1 identity and shared capabilities. Terminal
  authentication is kept: an object-valued `auth.terminal` becomes v1's
  `auth.terminal: true`. Elicitation capabilities and an object `_meta` are
  kept when they parse under v1. The SDK never enables v1 file-system or
  terminal-execution capabilities from unknown newer fields.
  V1's original fallback-to-latest rule is retained, including a v1 selection
  when the client requests an unsupported older version.
- A v1 agent that registers a typed `initialize` request handler rejects any
  other request that arrives before initialization completes with `-32600`.
  Agents without one, including those with only catch-all handlers, keep
  serving requests without initialization. V1 agents still accept a repeated
  `initialize` at any time.
- A standalone v2 agent accepts a request for v2 or a newer version and
  selects v2. The request delivered to the handler has `protocol_version`
  normalized to v2, and the response selects v2. A request for exactly v2 must
  be a valid v2 request. A request for a newer version was written for a
  version the agent doesn't know, so the SDK keeps what parses under v2
  instead of rejecting it: capabilities that don't parse and a `_meta` that is
  not an object are dropped, fields v2 doesn't define are dropped, and a
  missing or malformed `info` is replaced by an implementation named `unknown`
  with version `unknown`. A v1 agent downgrading a newer request applies the
  same rule to v1's shape, leaving its optional `clientInfo` unset instead. If a client initializes with v1, the request resolves with
  `-32022` (`Unsupported protocol version`) and
  `data: { "requested": 1, "supported": [2] }`. The v2 initialization handler
  is not invoked. Use an agent router with a v1 implementation to serve v1
  sessions.
- If the agent responds with any other unsupported version, the request resolves
  with an error so the client can close the connection.
- After initialization, the local API version and negotiated wire version must
  match. The SDK does not convert traffic between v1 and v2.

That means v1 and v2 implementations still need separate handlers.
`Agent.v2()`, `Client.v2()`, and `Proxy.v2()` are v2-only. While protocol v2
stabilizes, the `unstable_protocol_v2` crate feature also exposes
`Agent.protocol_router()`, `Proxy.protocol_router()`, and
`Client.protocol_connector()` for composing version-specific implementations.

Keep these protocol handlers as adapters around shared application logic, not
as general-purpose conversions between wire types. In particular, v1 prompt
responses end a turn, while v2 prompt responses acknowledge insertion and
completion arrives through session state updates. Shared storage also does not
automatically make sessions resumable through both versions. See the protocol's
[dual-stack migration guidance](https://agentclientprotocol.com/protocol/v2/migration#supporting-v1-and-v2-side-by-side)
for adapter boundaries and a validation checklist.

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
let v1_agent = Agent.v1().on_receive_request(
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

The agent protocol router reads the initial `initialize` request and selects
a version-specific implementation. A request for v2 or a newer version selects
the v2 implementation. When a v1 implementation is configured, everything else
is handed to it unchanged: a request for v1 or an older version, a malformed
`initialize`, and any other first message. A v1 client therefore sees exactly
the v1 implementation, including v1's original fallback-to-latest rule, as it
would without `unstable_protocol_v2`. A v2-only endpoint rejects a request
below v2 with a structured version-mismatch error. The router
normalizes a v2 initialize request when selecting a v1 implementation, but does
not convert messages between v1 and v2 after routing. The selected
implementation owns the connection: a repeated `initialize` is forwarded to it
and never re-routed, so it is evaluated against the selected version. A v2
implementation rejects a repeat that requests v1 with `-32600`, and a v1
implementation selects v1 for a repeat that requests a newer version. The
router preserves the complete initial frame when handing it to the selected
implementation. A batched first `initialize` gets one `-32600` error with
`id: null`, and the client may send `initialize` again. Response-only frames
before initialization are ignored.

### Version mismatch errors

A v2-only endpoint rejects a v1 request without requiring v2's Client identity
or manufacturing a higher-version success response. The error code is
`-32022`, and its data contains `requested` and the endpoint's `supported`
versions. This diagnostic is separate from malformed initialization parameters,
which remain `-32602`. The router answers with the request's ID and does not
start an application. A failed initialize leaves the connection
uninitialized and open, so the client may send `initialize` again, for
example with a supported version or corrected parameters. Notifications that
arrive before initialization are ignored. The proxy protocol router, whose
`proxy/initialize` comes from the conductor, also answers a version mismatch
with `-32022` but closes after a rejection.

Custom endpoints can construct the same wire error with
`schema::v2::Error::unsupported_protocol_version(requested, supported).into()`.
The code is defined by v2 negotiation, not by a change to the v1 protocol.
The SDK's version-neutral `Error` carries any numeric code, so a v1 client
receives the initialization error as an unknown code without new v1 schema
definitions.

`just test` runs the negotiation tests in both the all-features workspace and
the core crate without default features, so v1 negotiation does not depend on
whether the draft v2 API is compiled in.

Clients that cannot use any supported version should close after a version
mismatch and display the supported versions. A successful downgrade must also be checked against the Client's
actual supported versions, because advertising a latest version does not imply
support for every older version.

### Proxy and client routing

The proxy protocol router negotiates `proxy/initialize` like the agent
protocol router negotiates `initialize`: it selects the highest configured
version at or below the requested one, rewrites a request for a newer version
to keep what parses under the selected one, and hands the initial frame to
that implementation. A request for exactly the selected version is validated
and handed over unchanged.

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

The reuse probe is conservative: if parsing and serializing the raw v2 request
would change any parameter, reuse is disabled and fallback opens a fresh
connection. That does not turn an otherwise valid v2 request into an error.

## Schema changes in schema 1.5 through 1.14

The `unstable_protocol_v2` API follows the moving draft schema. Schema 1.5 added
semantic newtypes for paths, media types, IDs, and cursors, renamed
`DiffPatch.diff` to `DiffPatch.text`, and added terminal state and output update
types. Schema 1.7 removed the former schema-wide v1/v2 conversion API:
versioned implementations should remain separate, with
purpose-specific adapters at runtime boundaries where the required state and
policy are available. These are draft API changes rather than stable v1 wire
changes. See [Migrating to
v2.0](./migration_v2.0.md#draft-v2-schema-updates) for concrete source changes.

Schema 1.6 adds `Cancelled` tool-call and plan-entry statuses to draft v2.

Schema 1.7 stabilizes elicitation and terminal authentication, so neither
surface requires its former SDK feature flag. It also adds context compaction
updates (stable since schema 1.11). The SDK carries them through its
existing typed `session/update` routing in both protocol versions. V1 clients
advertise compaction support through `ClientSessionCapabilities::compaction`.

Schema 1.8 stabilizes optional programmatic tool-call names in both protocol
versions. No tool-name-specific feature is required. Draft v2 users only need
`unstable_protocol_v2`. In v1, an omitted or `null` name leaves the existing
value unchanged on updates. In v2, omission leaves it unchanged, `null` clears
it, and a string replaces it. V1 cannot express the explicit v2 `null` clear
operation.

Schema 1.11 stabilizes session notices and compaction. No surface-specific
feature is required. Draft v2 users still need `unstable_protocol_v2`.

In draft v2, failures after a prompt's user message is inserted end active work
with `StopReason::Error(ErrorStopReason)` on the idle state update, optionally
including a JSON-RPC error object. Failures before insertion remain error
responses to `session/prompt`. `StopReason` now serializes as an object tagged
by `stopReason`, flattened into `IdleStateUpdate`. Its `Other` variant takes
`OtherStopReason` rather than a string and preserves unknown payload fields.
V1 prompt responses are unchanged and do not accept an `error` stop reason.

Schema 1.11 also rejects malformed payloads that receivers act on rather than
silently dropping invalid entries or substituting defaults. This includes
session setup's MCP servers and additional directories, terminal launch
arguments and environment, and v2 replay cursors. Typed request decoding
reports invalid parameters (`-32602`) without invoking the handler. `null`
still means empty for the defaultable acted-on lists. Omission is accepted
only where the field is optional. Capability and display metadata retain their
lenient decoding.

### Schema 1.12

Client-provided system prompts are available through the independent
`unstable_session_system_prompt` feature. Use the complete lifecycle request
with `_from` helpers and negotiate the agent's capability first; see
[Client-provided system prompts](./protocol.md#client-provided-system-prompts).
Next Edit Suggestions, their document events, and their position-encoding
capabilities have been removed from both protocol versions.

Foreground states are aggregate snapshots, not per-prompt events:

- Report `running` when work starts, or `requires_action` if it starts blocked
  on user action. Agent-initiated workflows and local commands use the same
  lifecycle without needing a model call or a synthetic user prompt.
- `requires_action` means the foreground workflow as a whole is blocked.
  A pending permission or elicitation does not imply this state while other
  foreground work can progress or when it belongs to background activity.
  Once reported, send `running` when progress resumes or `idle` when work ends.
- When no foreground work remains, report `idle`; normally include a stop
  reason. Repeated snapshots, including an old stop reason, are idempotent,
  not additional completions, cancellations, or token usage.
- New sessions default to idle, but updates can precede the new-session
  response. Do not overwrite them with idle when that response arrives.
- Every successful resume must send a current-state snapshot after any replay
  and before its response, even without a replay cursor. Do not replay
  historical states as live activity or reset state to idle on the response.
  Testy and the echo example only resume sessions without foreground work, so
  their snapshot is idle without a new stop reason.
- A resume should also return the current `configOptions` in its response and
  send current session info through `session_info_update` after any replay and
  before responding. Like the state snapshot, this synchronizes current state
  rather than replaying historical configuration or metadata changes.

Prompt acceptance means logical user-message insertion with an ID and a
position, not receipt or queueing. Running may precede insertion. Rejection
before insertion requires an error response and no user-message updates for
that submission; it does not alone end foreground work. A pre-insertion
foreground failure can also require an error idle update. After insertion,
the response stays successful and failure is reported on idle instead.
This includes authentication failures: after insertion, `auth_required`
(`-32000`) belongs in the error stop reason, not an error prompt response.
Authenticating again does not restart ended work, and automatically retrying
an inserted prompt would insert another message and could repeat side effects.
When closing cancels active work, final updates and idle with `cancelled`
precede the close response. An uninserted prompt cancelled by close gets
`-32800`; an inserted one still gets its successful message ID.

Agents reject unsupported replay cursor types with `-32602`, rather than
substituting another cursor. Retained user messages preserve the prompt
response's ID across replay. Agent and thought message IDs may change across
reconnections: rebuild that history on full replay instead of deduplicating
only by ID. A chunk replay's empty-content primer clears only its replay ID.

Top-level `_meta` on upsert updates patches stored metadata by key: omission
leaves it unchanged, `null` clears it, and an object preserves omitted keys,
removes keys whose values are `null`, and replaces each other value whole.
Nested objects are not merged; nested nulls are ordinary data. An empty object
does not clear metadata. Replay uses the same rules and never implicitly resets
metadata. `ToolCallUpdate::apply_update` and `TerminalUpdate::apply_update`
apply this rule; use `schema::v2::apply_meta_update` for application-owned
message, session, compaction, or subagent projections. Metadata on chunks,
content, snapshots, and `plan_update` is not an entity patch.

Protocol v2 negotiation does not select every experimental feature. Operations
such as provider configuration require their advertised capabilities.
Informational notices, compaction, draft plan representations, subagent
updates, and idle token usage can use their documented capability-free
fallbacks; accepting an unknown variant is not a support advertisement.
Unknown capability fields are ignored, while malformed known discriminators
must not fall through to an unknown-variant fallback.

Client projections of tool calls and terminals should follow these rules:

- `tool_call_update` is an upsert keyed by `toolCallId`. On an existing tool
  call, omission leaves a field unchanged and `null` resets it to its default.
  A new ID starts from the defaults: `kind` is `other`, `status` is `pending`,
  `content` and `locations` are empty, and `title`, `name`, `rawInput`, and
  `rawOutput` are unset. Show a generic title while `title` is unset.
- Treat a tool call whose `status` is unrecognized as still in progress.
- A `tool_call_content_chunk` for a `toolCallId` the client has not seen
  creates the tool call with those defaults and then appends. Likewise a
  `terminal_output_chunk` for an unseen `terminalId` creates the terminal with
  unknown fields and empty output, then appends.
- Decode each terminal chunk independently and append the bytes in order.
  Ignore a chunk or `output` snapshot whose `data` is not valid base64,
  keeping the bytes already held; the rest of that `terminal_update` still
  applies. Agents should send all output before a
  concrete `exitStatus`, but clients still append output that arrives after
  it. Clients may cap how much output they retain, for example by dropping the
  oldest bytes; a later `output` snapshot replaces whatever they kept.
- In a diff, `changes` is authoritative for affected paths and operations.
  Follow `changes` when `patch` disagrees with it, and handle diffs without a
  `patch`.

For v2 `git_patch` diffs, every path must be absolute and have no `a/` or `b/`
prefix, including rename/copy header paths. Git quoting and `/dev/null` for
an added/deleted file's missing side still apply. Configure diff parsers not to
strip a leading path component, since that would drop part of an absolute
path, and take paths from `changes` or from the `---`, `+++`, rename, and copy
lines rather than by splitting the `diff --git` header. The SDK carries patch
text; it does not rewrite paths or compute diffs from snippets.

### Schema 1.13

Draft v2 type changes:

- v2 has its own `RequestId`, `Request`, and `Response` types. Request IDs are
  a string or an integer, never `null`; error responses may still use `null`
  when the request ID could not be determined.
- `AvailableCommand` is a `type`-tagged union. Build commands with
  `AvailableCommand::Command(Command::new(..))` and read them through
  `name()`, `description()`, and `meta()`. Unknown command kinds parse as
  `Other`; show them as plain commands by name and description, or ignore them.
- `ToolCallLocation.line` is a 1-based `NonZeroU32`. The builder still takes a
  `u32` and treats `0` as no line.
- `TerminalExitStatus.exit_code` is an `i64` holding the code the platform
  reports, so negative codes such as Windows NTSTATUS values survive.
- `ResourceLink.size` is a `u64` byte count; a negative value parses as unknown.
- `terminal_update` has an optional `size` with `cols` and `rows`, patched like
  the other terminal fields. Clients should render the output at that size
  when it is present.
- `RequestPermissionOutcome::Cancelled` carries a `CancelledPermissionOutcome`
  with optional `_meta`; `{"outcome": "cancelled"}` still parses.
  `AuthCapabilities` is now `ClientAuthCapabilities`.
- `DiffPatch`, `ToolCallPermissionSubject`, and `Icon` gain a `_meta` field
  (`meta`).
- Unknown variants keep their optional fields verbatim in `fields`.

`v2::SessionBuilder::with_mcp_server`, `v2::ResumeSessionBuilder::with_mcp_server`,
`v2::ForkSessionBuilder::with_mcp_server`, and `Proxy.v2().with_mcp_server`
declare an `McpServer::Acp` server. The agent must advertise
`session.mcp.acp` to accept it; otherwise a conforming agent rejects the setup
request with `-32602`. When the downstream agent only supports another MCP
transport, place a compatibility proxy such as `McpOverAcpPolyfill` in front of
it. The SDK does not check the agent's capability before sending the
declaration.

Client projections should also follow these rules:

- A `state_update` with an unrecognized state never means idle. Show a generic
  busy state until a known state arrives.
- An update or chunk for an unseen message ID creates the message. Its first
  update or chunk fixes its timeline position; later updates patch it in
  place, even after idle. Updates for different entities may interleave, and a
  new ID does not end any other entity.
- Show an unrecognized tool `kind` as `other`.
- A `tool_call` permission subject applies to the stored tool call exactly like
  a `tool_call_update`, in the order received and before the prompt is shown. A
  first-seen `toolCallId` creates the tool call.
- The Agent owns remembered `allow_always` and `reject_always` decisions and
  their scope. Clients do not infer that scope from the option kind.

### Schema 1.14

Draft v2 type changes:

- JSON-RPC batching is removed from v2, and `schema::v2::JsonRpcBatch` no
  longer exists. The SDK dropped batch support in both protocol versions:
  every connection answers a JSON array with a single `-32600` error with
  `id: null` (see [JSON-RPC batches](#json-rpc-batches)).
- `Error.data` is typed as `ErrorData`: `UnsupportedProtocolVersionErrorData`
  for `-32022`, `SessionNotFoundErrorData` for `-32002`, or any other JSON.
  `v2::Error::resource_not_found()` no longer takes a `uri`; use
  `session_not_found(session_id)` for a missing session.
  `v2::Error::unsupported_protocol_version` builds the typed data; convert
  either into the SDK's `Error` with `.into()`. An error response's `id` may
  be `null`.
- `UsageUpdate.cost` is a `MaybeUndefined<Cost>` patch field: omitted leaves
  the cost unchanged, `null` clears it, and a value replaces it.
  `Cost.currency` is any unit string, not only an ISO 4217 code.
- `ElicitationContentValue::Other` holds any JSON value. Validate accepted
  content per property against the requested schema: an unexpected value
  invalidates only that property.
- `ElicitationSchema` requires `type: "object"` and `properties` on the wire.
- `PromptRequest.prompt` must not be empty, and select config options and
  option groups must not be empty. Clients should hide a group that becomes
  empty after skipping values they cannot parse.
- `SessionInfo.additionalDirectories` treats `null` as `[]`, and
  `Annotations.lastModified` accepts any ISO 8601 string.
- With `unstable_subagents`, subagent updates gain `cwd` and
  `additional_directories` patch fields (absolute paths; omitted leaves them
  unchanged, `null` inherits the parent's).

Connection and lifecycle rules:

- A failed `initialize` leaves the connection uninitialized, and the client
  may send `initialize` again. `Agent.protocol_router()` now keeps the
  connection open after rejecting a first message. Agents ignore
  notifications that arrive before initialization.
- Each side picks its own request IDs, so the same ID may be in flight in both
  directions. `$/cancel_request` names a request its sender issued, and is
  ignored for unknown or already-answered requests. Unknown notifications,
  including `$/`-prefixed ones, are ignored. A connection's end fails its
  unanswered outgoing requests.
- `session/close` is idempotent: closing an unknown or already-closed session
  succeeds. `v2::ActiveSession::close` treats a `-32002` response as success, and
  Testy and `simple_agent_v2` close idempotently.
- Closing cancels the session's work like `session/cancel`, cancels pending
  elicitations scoped to it, answers held prompts with `-32800`, and the agent
  sends nothing more for that session afterwards. Deleting an active session
  first closes it.
- `cwd` must be absolute in the native syntax of the agent's machine, and
  agents reject a relative `cwd` in `session/new`, `session/resume`, and the
  `session/list` filter with `-32602`. That filter matches only the exact
  directory. Agents that advertise `additionalDirectories` reject relative
  entries; agents that do not ignore the field. Testy and `simple_agent_v2`
  follow these rules.
- MCP server names must be unique within `mcpServers`, and agents reject
  duplicates with `-32602`. The v2 `with_mcp_server` builders fail with
  `-32602` instead of declaring a server whose name is already in the
  request. On `session/resume`
  and `session/fork`, `mcpServers` is the complete set for the session;
  agents do not restore servers from earlier requests. A resume must use the
  session's `cwd`.
- `session/new`, `session/resume`, and `session/fork` responses carry the
  complete `configOptions` and available commands when the session has any.
  Clients replace their lists with them, including lists from updates that
  arrived before the response.
- An empty-string `nextCursor` from `session/list` is a valid cursor; only an
  omitted or `null` cursor ends the results.

Client projections should also follow these rules:

- A `plan_update` replaces the whole plan with that `planId`, even when its
  `type` changed to one the client does not understand.
- A `cancelled` permission outcome, or a `selected` outcome whose `optionId`
  was not offered, means not approved. Clients send `cancelled` when the user
  dismisses the request or the client declines it by policy.
- `completed`, `failed`, and `cancelled` tool call statuses are final, and
  agents finalize unfinished tool calls when foreground work ends.
- A prompt invokes a slash command only when its first text block starts with
  `/` and the command name followed by whitespace or the end of the block.
- Show a placeholder for content you cannot render rather than dropping it.
- A terminal's `size` is a rendering hint.
- `_meta` keys `traceparent`, `tracestate`, and `baggage` use the W3C formats;
  extensions use reverse-DNS prefixes such as `com.example/requestId`.
