# Protocol V2

The core SDK can opt into ACP protocol v2 with the `unstable_protocol_v2` crate
feature. Protocol v2 has been in Preview since schema 1.20: it is ready to build
on, and it is not frozen. The SDK's v2 API can still change in a minor release.

```toml
agent-client-protocol = { version = "...", features = ["unstable_protocol_v2"] }
```

This feature is separate from the broad `unstable` feature because protocol v2
is a separate protocol version, not an unstable method family.

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
`agent_client_protocol::v2` and message types from `v2::schema`. Importing
`agent_client_protocol::v2::{self, schema}` brings both into scope without a
name clash. The preceding agent fragment demonstrates version negotiation only.
An agent that advertises session support must also implement the complete
baseline session surface shown in the runnable quickstart.

## `tool_call/cancel` and `terminal/cancel`

Neither method has an initialize-time capability. An agent that advertises
`capabilities.cancel` on a tool call or terminal must implement the corresponding
request. The SDK answers an unhandled method with `-32601` (Method not found).
Before initialization these requests get `-32600` like any other.

If you implement the method, respond with `-32002` and `data.sessionId` for an
unknown or inactive session. An announced child of an active session is not
unknown. For a known session, respond with `-32602` (Invalid params) if the tool
call or terminal is unknown, final, or not cancellable.

`sessionId` is the session that reports the tool call or terminal. For a
subagent's tool call or terminal, that is the child session. The item's own
`capabilities.cancel` enables the request even when the child session doesn't
advertise `cancel`, and it enables nothing else on the child. Send
`v2::schema::CancelToolCallRequest` or `v2::schema::CancelTerminalRequest` with
the child's `sessionId`.

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
the Rust payload type: callers on a v2 connection must use `v2::schema` types,
or deliberately send extension or untyped messages. The connection guard
enforces negotiation and initialization lifecycle, not Rust-type provenance.
Dynamic handlers registered through `v2::ConnectionTo::add_dynamic_handler` use
the same low-level `HandleDispatchFrom` interface and therefore also receive
`ConnectionTo<_>`. Raw code converts explicitly: `connection.into_protocol::<V1>()`
returns a `v1::ConnectionTo` (and `into_protocol::<V2>()` a `v2::ConnectionTo`),
and either versioned connection's `into_raw()` returns the version-neutral one.
`into_protocol` fails with `-32603` when the connection's builder guards a
different version, so a connection from `Client.v1()` never becomes a
`v2::ConnectionTo`. `ConnectionTo::protocol_version()` reports the guarded
version, or `None` for an unguarded `Builder::new` connection.

`spawn_connection` returns the connection type the child builder's callbacks
receive, on the raw `ConnectionTo` and on both versioned connections:
spawning `Client.v2()` returns a `v2::ConnectionTo<_>`, spawning
`Client.v1()` a `v1::ConnectionTo<_>`, and spawning a raw `Builder::new`
builder a raw `ConnectionTo<_>`.

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
  response stays pending until insertion. `cancel_active_work` and `close`
  drop the prompts the agent has received and not inserted. Cancelling a held prompt's
  `SentRequest` sends `$/cancel_request`, which is best effort: the agent
  either answers `-32800` or inserts the prompt as usual. An inserted prompt
  cannot be withdrawn.
- `send_prompt_blocks_with_key` sends the prompt with an `idempotencyKey`.
  While the agent recognizes the key, a retry with the same key does not
  insert a second user message. A resumed session does not tell the client
  which keys the agent kept, so a retry after a reconnect can still insert one
  (see [Schema 1.20](#schema-120)). `send_prompt` and `send_prompt_blocks` send
  no key.
- `cancel_active_work` sends session-wide `session/cancel`. It covers the
  session's foreground work and prompts received before it that the agent has
  not yet inserted; those prompt requests fail with `-32800` (request
  cancelled), while a prompt whose user message was already inserted still
  succeeds with its message ID. A prompt sent after it is not cancelled.
  Work in the latest `background` list is not covered and keeps running; the
  agent may stop background activity it does not list.
  Cancellation of active foreground work completes after the required `idle`
  update with stop reason `cancelled`, which the agent sends before it starts
  new foreground work in the session. Updates for listed background work can
  still arrive after it. Cancelling an idle session need not emit another
  idle update, and work that had already ended keeps its stop reason.
  `cancelled` is reserved for work the client cancelled with `session/cancel`
  or by closing the session, and for a child session whose parent cancelled
  its work. Work the agent stops on its own uses another reason, such as
  `error`. A prompt sent while cancellation is in progress is not rejected for
  that reason: the agent inserts it, or holds it until the cancelled work has
  reported `idle`. The client should immediately mark unfinished tool calls for the active work as
  cancelled. The agent owns pending permission requests: it stops waiting
  for those of the cancelled work, treats them as not approved, and must
  cancel each with `$/cancel_request`. The agent decides which requests
  belong to that work, so the client does not answer every pending
  permission request when it sends `session/cancel`. The client should
  dismiss a cancelled permission request and answer it with the `cancelled`
  outcome, and may answer one it knows belongs to the cancelled work that way
  without waiting. Permission requests for listed background work stay open.
  Elicitations are not all cancelled implicitly: the agent must cancel each
  elicitation it stops waiting for with an individual `$/cancel_request`, and
  the client should answer it with the `cancel` action. Cancelling or dropping the prompt's `SentRequest` is the
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
order. The agent ends the replayed history with a `replay_complete` update
(see [Schema 1.18](#schema-118)). Do not send a second `session/resume` with a
`replayFrom` cursor for a session while the first one is pending. If a handler forwards updates to another
task, the application is responsible for any additional projection-drained
barrier it needs before treating replay as locally applied.
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
use agent_client_protocol::v2::{SessionSetup, schema};
use agent_client_protocol::{Client, Proxy};

Proxy
    .v2()
    .on_receive_request_from(
        Client,
        async |request: schema::NewSessionRequest, responder, cx| {
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

Proxy implementations use the schema crate's
`agent_client_protocol::v2::schema::InitializeProxyRequest`. Its response is
`v2::schema::InitializeProxyResponse`, a transparent wrapper around the v2
`InitializeResponse`. `v1::schema::InitializeProxyRequest` is the v1
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
  flight, other requests are rejected as they are during the first. Each
  repeat is negotiated like the first `initialize`: a v2 agent selects v2 for a
  request for v2 or newer, and answers a request below v2 with `-32022` and
  `data: { "requested", "supported": [2] }`. A repeat that fails, whether
  rejected by the SDK or by the handler, keeps the previous initialization in
  place and the client may repeat again. The first
  other request closes this window: a later `initialize` is rejected with
  `-32600` on both sides and the connection stays ready. Notifications such as
  `$/cancel_request` do not close it.
- V2 request ids must be strings or integers. A v2 connection answers a request
  with a `null` id with `-32600` and `id: null`. V1 connections still accept
  `null` ids.
- Since schema 1.13, v2 has its own `v2::schema::RequestId` without a `Null`
  variant. Request handles carry a protocol marker as their last type
  parameter: `Responder<T, V2>`, `ResponseRouter<T, V2>`,
  `SentRequest<T, V2>`, `PreparedRequest<T, V2>` and
  `Dispatch<Req, Notif, V2>`, named `v2::Responder<T>`,
  `v2::ResponseRouter<T>`, `v2::SentRequest<T>`, `v2::PreparedRequest<T>` and
  `v2::Dispatch<Req, Notif>`. Handles from `Client.v2()`, `Agent.v2()`,
  `Proxy.v2()` and `v2::ConnectionTo` use `V2`, so their `id()` returns a
  `v2::schema::RequestId`, for example to match an elicitation's `requestId`.
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
  the selected version, an `InitializationVersionMismatch` whose `response` is a
  `SelectedInitializeResponse::V1`. Both types are at the crate root because
  they are not tied to one protocol version. The `Initialized` and `V1` variants
  each hold a boxed `InitializeResponse` from the selected schema. Read its
  fields directly, or use `*response` to take the owned response out of the box.
  After a lower selection the connection stays uninitialized, and the caller can
  explicitly fall back to a v1 client implementation if that is acceptable. Errors
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
use agent_client_protocol::{v1, v2};
use agent_client_protocol::{Agent, ConnectTo};

# fn implementation() -> v2::schema::Implementation {
#     v2::schema::Implementation::new("example", "0.1.0")
# }
# async fn serve(client_transport: impl agent_client_protocol::ConnectTo<Agent>) -> agent_client_protocol::Result<()> {
# let enable_protocol_v2 = true;
let v1_agent = Agent.v1().on_receive_request(
    async |initialize: v1::schema::InitializeRequest, responder, _cx| {
        responder.respond(v1::schema::InitializeResponse::new(initialize.protocol_version))
    },
    agent_client_protocol::on_receive_request!(),
);

let agent = Agent.protocol_router().with_v1(v1_agent);

let agent = if enable_protocol_v2 {
    let v2_agent = Agent.v2().on_receive_request(
        async |initialize: v2::schema::InitializeRequest, responder, _cx| {
            responder.respond(v2::schema::InitializeResponse::new(
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
and never re-routed, so it is negotiated like a first `initialize` against the
selected version alone. A v2 implementation answers a repeat that requests v1
with `-32022` listing only v2, because the connection cannot switch to the v1
API, and the v2 initialization stays in effect. A v1 implementation selects v1
for a repeat that requests a newer version. The
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
`v2::schema::Error::unsupported_protocol_version(requested, supported).into()`.
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
  connection. The protocol permits requesting the lower selected version in
  that initialization.
- If the agent rejects the v2 initialize request, the error is surfaced. A
  rejected initialize is not treated as permission to retry with v1.

The reuse probe is conservative: if parsing and serializing the raw v2 request
would change any parameter, reuse is disabled and fallback opens a fresh
connection. That does not turn an otherwise valid v2 request into an error.

## Schema changes in schema 1.5 through 1.21

The `unstable_protocol_v2` API follows the v2 schema as it changes. Schema 1.5 added
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

Client-provided system prompts were added behind the
`unstable_session_system_prompt` feature. Schema 1.16 stabilizes them and the
feature has been removed. Use the complete lifecycle request with `_from`
helpers and negotiate the agent's capability first; see
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
  and its `replay_complete`, and before its response, even without a replay
  cursor. Do not replay historical states as live activity or reset state to
  idle on the response.
  Testy and the echo example only resume sessions without foreground work, so
  their snapshot is idle without a new stop reason.
- A resume must also return the session's current `configOptions` and
  available commands in its response when it has any, because the response
  replaces the client's lists. It should send current session info through
  `session_info_update` after any replay and before responding. Like the state
  snapshot, this synchronizes current state rather than replaying historical
  configuration or metadata changes.

Prompt acceptance means logical user-message insertion with an ID and a
position, not receipt or queueing. Running may precede insertion. Rejection
before insertion requires an error response and no user-message updates for
that submission; it does not alone end foreground work. A pre-insertion
foreground failure can also require an error idle update. After insertion,
the response stays successful and failure is reported on idle instead.
This includes authentication failures: after insertion, `-32000`
(Authentication required) belongs in the error stop reason, not an error
prompt response.
Authenticating again does not restart ended work, and automatically retrying
an inserted prompt would insert another message and could repeat side effects.
When closing cancels active work, final updates and idle with `cancelled`
precede the close response. An uninserted prompt cancelled by close gets
`-32800`; an inserted one still gets its successful message ID.

On `session/cancel`, the agent must stop waiting for the pending
`session/request_permission` requests of the cancelled foreground work and
treat them as not approved. It must cancel each of them with
`$/cancel_request` (a recommendation before schema 1.19): call
`SentRequest::cancel` or `RequestCancellationHandle::cancel`, or drop the
`SentRequest`, which sends `$/cancel_request` and discards the late answer.
The agent decides which requests belong to the cancelled work. Requests for
work in the latest `background` list are not affected. Before responding to
`session/close`, the agent must cancel every pending permission request and
elicitation in the session with `$/cancel_request`. Protocol v1 is unchanged:
there the client must answer every pending permission request with
`cancelled` when it sends `session/cancel`.

Agents reject unsupported replay cursor types with `-32602`, rather than
substituting another cursor. Retained user messages preserve the prompt
response's ID across replay. Agent and thought message IDs may change across
reconnections, so do not deduplicate a replay against history you already
hold by ID. A client can rebuild its transcript from a full replay, which
holds the history the agent retained and not necessarily everything the client
saw live. A chunk replay's empty-content primer clears only its replay ID.
Since schema 1.19 the primer applies to every entity replayed as chunks, not
only to messages (see [Schema 1.19](#schema-119)).

Top-level `_meta` on upsert updates patches stored metadata by key: omission
leaves it unchanged, `null` clears it, and an object preserves omitted keys,
removes keys whose values are `null`, and replaces each other value whole.
Nested objects are not merged; nested nulls are ordinary data. An empty object
does not clear metadata. Replay uses the same rules and never implicitly resets
metadata. `ToolCallUpdate::apply_update` and `TerminalUpdate::apply_update`
apply this rule; use `v2::schema::apply_meta_update` for application-owned
message, session, usage, compaction, or subagent projections. Metadata on chunks,
content, snapshots, and `plan_update` is not an entity patch.

Protocol v2 negotiation does not select every experimental feature. Operations
such as provider configuration require their advertised capabilities.
Informational notices, compaction, plan updates, subagent updates, and idle
token usage can use their documented capability-free fallbacks; accepting an
unknown variant is not a support advertisement.
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
  Agents should send all output before a
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
  `Other`. Ignore them, and do not show them as slash commands. Schema 1.13
  allowed showing them as plain commands. Schema 1.18 forbids it, because each
  command type defines how its entries are invoked.
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

- JSON-RPC batching is removed from v2, and `v2::schema::JsonRpcBatch` no
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
- `ElicitationContentValue::Other` holds any JSON value, such as the value of
  a custom or future property type. Validate accepted content against the
  requested schema yourself.
- `ElicitationSchema` requires `type: "object"` and `properties` on the wire.
- `PromptRequest.prompt` must not be empty, and select config options and
  option groups must not be empty.
- `SessionInfo.additionalDirectories` treats `null` as `[]`, and
  `Annotations.lastModified` accepts any ISO 8601 string.
- Subagent updates gain `cwd` and `additional_directories` patch fields
  (absolute paths; omitted leaves them unchanged, `null` inherits the
  parent's). They needed `unstable_subagents` until schema 1.18.

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
  elicitations scoped to it, and answers held prompts with `-32800`. After the
  close response the agent sends no notifications or requests for that
  session, but it still answers `session/prompt` requests that were pending.
  Deleting a session first cancels its work and frees its resources, as if it
  had been closed when it is active.
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
  This applies to `command` entries only.
- Show a placeholder for content you cannot render rather than dropping it.
- A terminal's `size` is a rendering hint.
- `_meta` keys `traceparent`, `tracestate`, and `baggage` use the W3C formats;
  extensions use reverse-DNS prefixes such as `com.example/requestId`.

### Schema 1.18

Draft v2 type changes:

- `SessionUpdate::ReplayComplete` marks the end of the history replay that a
  `session/resume` request asked for. `ReplayComplete` has only an optional
  `_meta`.
- Subagents are part of v2 and no longer need `unstable_subagents`:
  `SessionUpdate::SubagentUpdate`, `SessionUpdate::SessionMessage`,
  `SessionUpdate::SessionMessageChunk`, `StateUpdate::Unknown`,
  `BackgroundWork::Subagent`, and `ToolCallContent::Session` are always
  available. These wire values used to parse as `Other` without the feature
  and now parse as their typed variants, so add match arms where you relied on
  the fallback. v1 needed `unstable_subagents` until schema 1.20.
- The `file` plan type is removed (plan operations needed
  `unstable_plan_operations` until schema 1.20):
  `PlanUpdateContent::File`, `PlanFile`, and `PlanUpdateContent::file` no
  longer exist, and a received `file` plan parses as
  `PlanUpdateContent::Other`. `PlanMarkdown` gains an optional `path`, the
  absolute path of the file that holds the plan. Build it with
  `PlanUpdateContent::Markdown(PlanMarkdown::new(..).path(..))`.
- With `unstable_agent_skills`, `AvailableCommand::Skill` advertises an Agent
  Skill and `ContentBlock::Skill` (`SkillReference`) invokes one in a prompt.
  The agent advertises `PromptCapabilities::skill`, and the client advertises
  `session.commands.skill`.
- With `unstable_prompt_suggestions`,
  `SessionUpdate::PromptSuggestionsUpdate` replaces the session's list of
  prompts the agent predicts the user will send next. The client advertises
  `session.promptSuggestions`.
- `ClientCapabilities::session` (`ClientSessionCapabilities`) exists with
  either of those two features. It only groups capabilities: `{}` advertises
  nothing.
- With `unstable_speed_config_category`, `SessionConfigOptionCategory::Speed`
  marks a response speed option such as a fast mode toggle.
- With `unstable_session_fork` and `unstable_session_options`,
  `ForkSessionRequest::config_options` carries initial config option values.
  The entries apply on top of the configuration the fork would start with.

Agents must follow these replay rules:

- For a `session/resume` request with a `replayFrom` cursor, send
  `replay_complete` exactly once on the session, after the last replayed
  update and before current-state synchronization and the response. Send it
  for an empty replay too.
- Do not send it when `replayFrom` is omitted or `null`, for `session/new`,
  when the cursor is rejected with `-32602`, or when the replay cannot finish.
- From the time you receive the request until you send `replay_complete`,
  every update you send for the session must be replayed history. Hold
  everything else until after it: chunks of a
  message that is still streaming, notices, `available_commands_update`,
  `config_option_update`, and the current session info and state snapshots.
  Permission requests and elicitations should wait too.
- Each child session announced with `subagent_update` during the resume gets
  its own `replay_complete` on its own `sessionId`, after its last replayed
  update. The markers of different sessions have no required order.
- `replay_complete` does not make the session active. A session that was not
  active before the request becomes active with the successful response, so
  `session/prompt` fails with `-32002` until then.
- Report known gaps in the retained history after `replay_complete`, for
  example with a notice.

Testy and `simple_agent_v2` follow these rules.

Other agent rules:

- `auth/logout` is idempotent. Respond with the same empty result when there
  is no authenticated state to end.
- Answer a repeated `tool_call/cancel` or `terminal/cancel` with `-32602` once
  the tool call is final or the terminal has exited. The `tool_call_update` or
  `terminal_update` reports the outcome.
- List the values of a `select` `thought_level` option from lowest to highest,
  and of a `select` `speed` option from slowest to fastest. In a `boolean`
  `speed` option, `true` is the faster setting.
- `content` on `tool_call_update` replaces the whole array, so send the
  `session` references again when you send new content. Add a reference only
  for a handle you can resolve to a known session.
- Omit `senderSessionId` on a session-directed message when you have no
  evidence of who sent it. Do not assume the parent.
- Report `unknown` on an actual loss of observability, not after silence.
  Leave unfinished tool calls and terminals as last reported, and update
  capabilities explicitly when a control is no longer available.
- Send `skill` command entries only to a client that advertises
  `session.commands.skill`, and advertise `session.prompt.skill` when you do.
  Reject a prompt with a `skill` block that your capability does not allow, or
  whose `name` you cannot resolve, with `-32602` and without inserting it.
- Send `prompt_suggestions_update` only to a client that advertises
  `session.promptSuggestions`. When you accept a `session/prompt`, replace a
  non-empty list or withdraw it with an empty one. Do not replay suggestions.
- While applying initial `configOptions`, do not send `config_option_update`
  for them on the requesting connection, and send other updates they cause
  only after the `session/new` or `session/fork` response. A resumed session
  that is active on other connections does get `config_option_update` there.
- `session/options` may start the agent's runtime to resolve the options. It
  still stores no session and writes no history.

Client projections should also follow these rules:

- Use `replay_complete` to find the end of the replay you requested. Every
  update for the session after it is not history. Sending `session/resume`
  does not mark the start of the replay: when the session is already active on
  the connection, a live update the agent sent before it received the request
  can still arrive before `replay_complete`.
  A resume that ends in an error response or a closed connection without
  `replay_complete` delivered a truncated replay. Do not treat those updates
  as a complete replay. A successful response without `replay_complete` is a
  protocol violation, but the response still confirms that the requested
  replay finished.
- Ignore command types you do not understand, and never show them as slash
  commands. The `/name` syntax invokes `command` entries only. Invoke a skill
  with a `skill` content block, and only where the agent's
  `session.prompt.skill` capability allows it: one leading block, or any
  number at any position with `inline`.
- On `unknown`, stop presenting a previous `running` or `requires_action` as
  confirmed live activity. It is not `idle`, not an outcome, and not a
  cancellation. A later known state replaces it.
- Session-directed messages are agent content. Attribute them to the sending
  session and do not present them as user messages.
- Render a markdown plan from `content`. Use `path` only to open the file, and
  do not read a `path` outside the session's roots without user action.
- Skip a whole prompt suggestion you cannot use rather than dropping one of
  its blocks. Never send a suggestion as a prompt without an explicit user
  action.
- Render `speed` options near the model selector.
- Do not call `session/options` on a timer.

### Schema 1.19

Type changes:

- `PromptRequest::idempotency_key` (`idempotencyKey`) identifies one
  `session/prompt` submission across attempts. `IdempotencyKey` is an opaque,
  case-sensitive string of 1 to 255 characters. `IdempotencyKey::new` and
  deserialization reject any other value, so a typed handler never sees a
  malformed key: the SDK answers that request with `-32602`.
  `unstable_protocol_v2` also enables `IdempotencyKey::generate`, which
  creates a key from a random UUID.
- `StringPropertySchema::pattern` is removed from draft v2. A string property
  in an elicitation form takes `minLength`, `maxLength`, `format`, and
  `default`. A `pattern` on a received form is ignored and not kept. v1 keeps
  `pattern`.
- `Diff.changes` has `minItems: 1` in the JSON Schema. The SDK still parses
  an empty list.
- With `unstable_session_cursors`, `SessionUpdate::CursorUpdate` announces a
  session cursor: one position in the session's timeline where the client can
  run a method. `CursorUpdate` carries a `CursorId`, a `CursorCapabilities`
  patch, and a `CursorAnchor` that names a message, tool call, or compaction
  and the side of it the cursor is on. With `unstable_session_fork` as well,
  `ForkSessionRequest::cursor_id` forks the session at a cursor that
  advertises `capabilities.fork`. Both exist in v1 and draft v2. v1 clients
  opt in with `ClientSessionCapabilities::cursors`. Draft v2 needs no
  capability, and without the feature `cursor_update` parses as
  `SessionUpdate::Other`.
- The schema's aggregate enums (`ClientRequest`, `AgentResponse`,
  `AgentNotification` and the others, in both versions) gain
  `from_method_and_params`, `from_method_and_result`, and `supports_method`,
  with `schema::MethodDispatchError` as their error. They select the payload
  type from the JSON-RPC method. SDK connections already parse messages by
  method through `JsonRpcMessage::parse_message`, so these are for code that
  parses messages without a connection. Do not deserialize an aggregate enum
  directly: its untagged `Deserialize` selects a variant by payload shape.

Agents must follow these `session/prompt` retry rules:

- Support `idempotencyKey` when you advertise `capabilities.session`. There is
  no separate capability.
- Look the key up in the session named by `sessionId`. With nothing recorded,
  handle the prompt as usual and record the key in the same step as the
  inserted user message.
- For a key recorded with an inserted message, insert nothing, start no second
  round of work, and respond with that message's `messageId`.
- For a key whose prompt is not inserted yet, such as a held prompt, do not
  hold a second prompt. Answer the retry when you answer the first request,
  with the same outcome.
- An error response means the message was not inserted. Record nothing, so
  the client can send the same prompt with the same key again.
- If you honor `$/cancel_request` for any pending request with a key, withdraw
  the prompt if it is not inserted yet and answer every pending request for
  that key with `-32800`.
- A key is scoped to its session. The same key in another session, including
  one that `session/fork` created from this one, names an unrelated
  submission.
- Never insert a second message for a recorded key. A request that reuses a
  recorded key with a different prompt is answered as a retry, or rejected
  with `-32602` if you compare prompts. Comparing is optional.
- Recognize a key for as long as you retain its message, across connections
  and restarts. For a message you do not retain, recognize the key while the
  session stays active on a connection.
- Do not echo keys in session updates, `session/list` results, or replayed
  history. A proxy forwards `idempotencyKey` unchanged.

Testy and `simple_agent_v2` follow these rules. They do not compare prompts.

Other agent rules:

- Include the session's `configOptions` and `availableCommands` in the
  `session/new` and `session/resume` responses when the lists are nonempty.
  If a new session is not idle, send its `state_update` after the
  `session/new` response.
- If the agent cannot apply a supplied `systemPrompt` because the value is
  unsupported or exceeds its limits, answer `-32602` without creating or
  resuming the session.
- `session/cancel` and `session/close` drop the prompts you have received and
  not inserted. Answer each with `-32800`. Honoring `$/cancel_request` for a held
  prompt is optional: either answer `-32800` and do not insert the message, or
  insert it as usual. When the connection ends, you may insert or discard the
  prompts you were holding.
- Cancel each permission request and elicitation you stop waiting for after
  `session/cancel` with `$/cancel_request`. This is now required.
- Before you replay an entity from its beginning as chunks, send that entity's
  update with the chunked field emptied: `content: []` on the message update
  for message and session-directed message chunks, `content: []` on
  `tool_call_update` for `tool_call_content_chunk`, `output: { "data": "" }`
  on `terminal_update` for `terminal_output_chunk`, and `summary: []` on
  `compaction_update` for `compaction_summary_chunk`. An entity you replay
  without chunks needs no such update.
- A resume without `replayFrom` synchronizes the session's state, not its
  conversation. Send the current `state_update`, and before it a current
  `terminal_update`, `tool_call_update`, or `subagent_update` for each item in
  its `background` list. You are not required to resend foreground work in
  progress.
- Do not move a tool call back to `pending`, also not with `"status": null`.
  `completed`, `failed`, and `cancelled` are final. Report further work on a
  new tool call.
- Include at least one entry in a diff's `changes`.
- Do not send `pattern` on string properties. Validate the submitted value
  yourself.
- A child session you announced with `subagent_update` is not an unknown
  session. `session/cancel`, `tool_call/cancel`, and `terminal/cancel` can
  name a child where its capabilities allow. Reject any other request that
  names a child with `-32602`. Do not return child sessions from
  `session/list`. Closing a session also stops its child sessions, at any
  depth.
- The `capabilities` and `state` in a replayed `subagent_update` are
  historical. After a replay, send a current `subagent_update` for each child
  in the `background` list after the parent's `replay_complete`.
- A session's `cost` includes the costs of its descendants.
- Announce a session cursor only for a position where at least one method
  works, and only after the entity it is anchored to. Never change a cursor's
  anchor. Reject a `cursorId` you did not issue for that session, no longer
  accept, or that does not support the method with `-32602`. Never act at
  another position and never fork the whole session instead. In v1, send
  `cursor_update` only to a client that advertises `session.cursors`, and send
  a `messageId` on every message you anchor to.

Client projections should also follow these rules:

- To let the agent recognize a retry of a prompt whose response was lost, send
  the prompt with `send_prompt_blocks_with_key`. Create one key per submission
  when the user submits the input, and store it with the pending input. Use a
  new key for a new submission, and never send a key with a different prompt.
  On a new connection, call `initialize` and `session/resume` before you
  retry. Schema 1.20 changed what a client does next: see
  [Schema 1.20](#schema-120).
- Without a key, or with a key the agent has forgotten, a `session/prompt`
  sent again can insert a second user message. `auth/login`, `auth/logout`, `session/resume`,
  `session/set_config_option`, `session/list`, `session/close`,
  `session/delete`, `tool_call/cancel`, and `terminal/cancel` can be sent
  again as they are. A second `session/new` creates a second session. Do not
  send `session/cancel` again later: a later copy can cancel newer foreground
  work or a newly held prompt. Retry `session/set_config_option` before you send
  further changes, because a retry sent after a later change to the same
  option overwrites it. Treat `-32002` from `session/delete` as a successful
  delete. A `tool_call/cancel` or `terminal/cancel` retry gets `-32602` once
  the tool call is final or the terminal has exited. See
  [Retrying requests](https://agentclientprotocol.com/protocol/v2/overview#retrying-requests).
- When `$/cancel_request` arrives for a `session/request_permission` or
  `elicitation/create` request you have not answered, dismiss the prompt and
  answer with the `cancelled` outcome or the `cancel` action. Observe it with
  `Responder::cancellation`. A responder dropped after the cancellation
  answers `-32800` instead. Also dismiss permission and elicitation prompts
  for a session you closed.
- Correlate a submitted prompt with the agent's `user_message` by the
  response's `messageId`, not by content. The agent owns the inserted message
  and may report different content. The client may display what it originally
  sent; replay carries the agent's content.
- A resume without `replayFrom` does not resynchronize messages or foreground
  tool calls. Keep the session's entities if you missed no updates, and
  request replay otherwise, for example after a lost connection.
- Tool call `content`, terminal `output`, and compaction `summary` follow the
  same order as message content: an update that carries the field replaces
  what the chunks accumulated, and later chunks append to it.
- Show the status from the latest `tool_call_update` you received.
- `refusal` only reports that the agent refused to continue. Do not remove or
  mark the user's message because of it. The v1 stop reason still means that
  the prompt and everything after it are left out of the next prompt.
- Do not evaluate `pattern` on string properties.
- Register the child session that a `subagent_update` announces, and attribute
  updates and requests with its `sessionId` to it. Send nothing for a child
  except responses to its requests, `session/cancel` while the child
  advertises `capabilities.cancel`, and `tool_call/cancel` or
  `terminal/cancel` for an item that advertises its own. When you resume the
  parent, discard the child state and capabilities you held and wait for
  current ones: a `subagent_update` before the parent's `replay_complete`
  carries historical `capabilities` and `state`. A permission request or
  elicitation can name a child session, so do not send `session/cancel` for
  the session a request names unless it is a session you created or a child
  that advertises `capabilities.cancel`.
- A session's `cost` already includes the costs of its descendants. Do not add
  a child session's cost to an ancestor's. A child without a reported cost has
  an unknown cost, not zero. Context window usage belongs to each session and
  is not summed. See
  [Subagents](https://agentclientprotocol.com/protocol/v2/subagents) for the
  other client rules: the patch semantics of `subagent_update`, inherited
  working directories, read-only child configuration, session-directed
  messages, and what to show after the connection ends.
- Build cursors from `cursor_update` by `cursorId`. Pass a cursor to a method
  only when the agent announced it on that session and it currently
  advertises that method's capability, and pass the `cursorId`, never the
  anchored entity's ID. Do not guess a place for a cursor whose anchor you do
  not hold or do not recognize. A full replay rebuilds the session's cursors,
  so discard the ones you held. A forked session does not inherit cursors.

### Schema 1.20

ACP v2 moved from Draft to Preview with this release, and its schema is
`2.0.0-beta.1`. The SDK's v2 API stays behind `unstable_protocol_v2`.

Type changes:

- Plan operations are stable in both protocol versions, and the
  `unstable_plan_operations` feature is removed. In v2,
  `PlanUpdateContent::Markdown`, `PlanMarkdown`, and
  `SessionUpdate::PlanRemoved` are always available and need no client
  capability. A `markdown` plan and a `plan_removed` update used to parse as
  `Other` without the feature and now parse as their typed variants, so add
  match arms where you relied on the fallback. In v1,
  `SessionUpdate::PlanUpdate`, `SessionUpdate::PlanRemoved`, and
  `ClientCapabilities::plan` need no feature.
- Subagent sessions are stable in v1, and the `unstable_subagents` feature is
  removed. v2 has included them since schema 1.18.
- Maps on the wire keep their key order. In v2 they are `indexmap::IndexMap`s
  instead of `BTreeMap`s: the `fields` of every `Other*` variant,
  `ElicitationSchema::properties`, and `ElicitationAcceptAction::content`.
  `SetProviderRequest::headers` (with `unstable_llm_providers`) was a
  `HashMap` and is an `IndexMap` in both versions. In v1, the `fields` of
  `OtherStateUpdate`, `OtherStopReason`, and `OtherCursorAnchor` are
  `IndexMap`s too. The other v1 maps are unchanged. Name the type as
  `agent_client_protocol::schema::IndexMap`, which the SDK re-exports since
  [schema 1.21](#schema-121). Where a function takes the map directly, such
  as `OtherMcpServer::new`, an array of pairs with `.into()` also works. `apply_meta_update` keeps the order of the remaining keys when
  it removes one.
- `CompactionUpdate::status` is a `MaybeUndefined<CompactionStatus>` patch
  field: omitted leaves the status unchanged, `null` resets it to
  `in_progress`, and a value replaces it. `CompactionUpdate::new` takes only
  the ID. Set the status with `CompactionUpdate::status`. v1 still requires
  `status`.
- `UsageUpdate::meta` is a `MaybeUndefined<Meta>` metadata patch, like the
  `_meta` of the other upsert updates. Apply it with `apply_meta_update`.
- `TerminalSize::cols` and `TerminalSize::rows` are `NonZeroU16`. A received
  `size` with a zero is parsed as no size, and the rest of the
  `terminal_update` still applies.
- A `RequestPermissionRequest` with an empty `options` list fails typed
  parsing. The SDK answers that request with `-32602` and the handler does not
  run.
- `CreateElicitationRequest` rejects a `requestId` sent together with
  `sessionId` or `toolCallId`, and a `null` in the scope field that is not
  used. `MultiSelectItems` is selected by `type`: the titled `anyOf` form has
  no `type`, and a `type` of `"string"` needs `enum`.
- `SystemPrompt.content` has `minItems: 1` in the JSON Schema. The SDK still
  parses an empty list.
- The v2 JSON Schema validates the JSON-RPC envelope by method and direction
  and rejects `"params": null`. The SDK omits `params` when a message has
  none. The schema also carries `x-state` and `x-scope` hints, which say what
  a message changes and which session, tool call, or request it belongs to.
  They add no Rust types.

Agents must follow these rules:

- When `session/cancel` stops active foreground work, send that work's
  pending updates and then `idle` with `stopReason: "cancelled"` before you
  start new foreground work in the session, including work you start
  yourself. Do not reject a prompt because cancellation is in progress.
  Insert and acknowledge it, or hold it with its response pending if its work
  would start right away. Listed background work is not covered, and its
  updates can follow the idle update. If the foreground work had already
  ended, its stop reason stands.
- A `selected` permission outcome is not approval by itself. Match `optionId`
  against the options you offered in that request. Do not perform the
  operation when the option's kind is `reject_once` or `reject_always`, when
  no option matches, or when you do not understand the kind or the outcome.
  Neither the option's `name` nor the spelling of its `optionId` overrides a
  rejection. An error response is not approval, and neither is a connection
  that ends before the client answers.
- Reject a stdio MCP server whose `command` is not an absolute path with
  `-32602`. When a server fails to launch or connect, continue without it and
  tell the user, for example with a notice.
- Reject a `session/close` or a `session/delete` that names a child session
  with `-32602`. A child's config options are read-only, because no child
  capability permits `session/set_config_option`.
- Give every config option a `configId` that is unique in `configOptions`,
  and every group a `groupId` that is unique in its option.
- Do not replay past `usage_update`, `session_info_update`,
  `config_option_update`, or `available_commands_update` notifications. They
  describe earlier values. Synchronize the current ones instead.
- During a replaying resume, hold live updates and requests for each child
  you announce until that child's `replay_complete`.
- Report `unknown` whenever you can no longer observe a session's foreground
  work. This applies to every session, not only to child sessions.
- A child session also reports `cancelled` when its parent cancels its work.
  A response that arrives after work was cancelled must not permit a new task
  for the same child.
- Return the sessions with the most recent activity first from
  `session/list`.
- Send `plan_removed` when a plan is cancelled, for example when the user
  rejects it. A finished plan does not need to be removed. The `path` of a
  markdown plan must be absolute.
- In an elicitation, omit the scope field you do not use instead of sending
  `null`, and do not send `toolCallId` with `requestId`.
- Every authentication method carries a `type`, a `methodId`, and a `name`.
  The `env` names of a terminal method should be unique. Otherwise the last
  entry with a name wins.

Testy and `simple_agent_v2` already follow the rules that apply to them. They
have no permission requests, MCP servers, config options, plans, or child
sessions.

Client projections should also follow these rules:

- A successful `session/resume`, with or without replay, does not tell you
  whether an unanswered prompt was inserted or whether the agent kept its
  `idempotencyKey`. If a duplicate would matter and you cannot rule one out,
  do not resend the prompt automatically. Tell the user that the prompt might
  already have run and let them decide. A retry sends the original prompt and
  key in a new `session/prompt` request. This replaces the earlier advice to
  resend a pending prompt as soon as the session is resumed.
- Accept `items` and `markdown` plans and `plan_removed`. Accepting a plan
  does not require showing it. A `plan_update` replaces the whole plan with
  that `planId`. Ignore a `plan_removed` for a `planId` you do not know.
- When the user picks an offered option, answer with `selected` and its
  `optionId`, also for a rejection. You can return the ID without
  interpreting the option's kind.
- Keep accepting updates after `session/cancel`, including updates for
  surviving background work after the `idle` update with
  `stopReason: "cancelled"`. To stop a listed tool call or terminal, send
  `tool_call/cancel` or `terminal/cancel` while the item advertises
  `capabilities.cancel`. `session/cancel` on a child does not stop a terminal
  in the child's `background` list.
- Adjacent text blocks form one continuous text. Join them without a
  separator before rendering, in messages and in tool call content.
- Tool call titles, session titles, and the names and descriptions of
  commands and config options are plain text. Do not render them as Markdown.
  A plan entry's `content` may contain Markdown.
- Answer `elicitation/create` with `-32602` when you cannot render the form.
- Follow a `nextCursor` from `session/list` with the same `cwd` you sent
  before.
- Call `auth/logout` only when the agent advertised authentication methods.
- After the agent selects a lower protocol version, you may send that
  version, or a lower one you support, when you repeat `initialize` or open a
  new connection. `Client.protocol_connector()` does this when it falls back.

### Schema 1.21

Schema 1.21 changes Rust types, not the wire format:

- `agent_client_protocol::schema::IndexMap` is the map type of the schema's
  ordered maps: the `fields` of every `Other*` variant,
  `ElicitationSchema::properties`, `ElicitationAcceptAction::content`, and
  `SetProviderRequest::headers`. The schema crate re-exports it from
  `indexmap`, and the SDK passes it on. If you added `indexmap` to your
  dependencies for schema 1.20 only to name this type, you can remove it.

  ```rust
  use agent_client_protocol::schema::IndexMap;
  use agent_client_protocol::v1::schema::{OtherStateUpdate, StateUpdate};
  use serde_json::json;

  let fields = IndexMap::from([("reason".to_string(), json!("rate_limited"))]);
  let state = StateUpdate::Other(OtherStateUpdate::new("_paused", fields));
  ```

- As of 1.21.1, two preview v2 variants take boxed payloads. When constructing
  `v2::schema::SessionConfigOptionValue::Other(other)`, use
  `SessionConfigOptionValue::from(other)` instead. For
  `v2::schema::ToolCallContent::Diff(diff)`, use `ToolCallContent::from(diff)`.
  Both conversions accept the unboxed payload. When matching an owned variant,
  `*other` or `*diff` gives you the owned payload again. The v1 types are unchanged.

There are no new rules for agents or clients.
