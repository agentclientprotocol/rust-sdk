# Migrating to v5

This page covers the breaking changes in `agent-client-protocol` 5.0 and the
crates released with it.

There is no 4.x of `agent-client-protocol`. All SDK crates now share one
version number. `agent-client-protocol-rmcp`, which was at 4.x, joined that
version group in this release, so the shared version had to start above 4.x.
Upgrade from 3.6 directly to 5.0.

## Stability

- The stable v1 API, everything that is not behind an `unstable_*` feature,
  follows semver: it changes incompatibly only in a major release.
- Everything behind an `unstable_*` feature may change in a 5.x minor release.
  This includes all of `agent_client_protocol::v2` and the other items that
  `unstable_protocol_v2` enables. Stabilizing protocol v2 only removes the
  `unstable_protocol_v2` gate.
- A major release of a public dependency (`axum`, `reqwest` or `rmcp`) forces a
  major release of every crate in the shared version group.
- `agent-client-protocol` depends on one exact version of
  `agent-client-protocol-schema` (an `=` requirement), and a minor SDK release
  may move that pin to a newer schema release. Use the schema types through
  `agent_client_protocol::schema`, `v1::schema` and `v2::schema` instead of
  depending on `agent-client-protocol-schema` directly. A direct dependency
  must allow the exact version the SDK pins, or Cargo cannot resolve the
  dependency graph.

## `v1` and `v2` modules and versioned entry points

Every connection now names its protocol version, in the entry point and in
the imports:

- `Client.v1()`, `Agent.v1()` and `Proxy.v1()` start a stable protocol v1
  connection. They replace `Client.builder()`, `Agent.builder()` and
  `Proxy.builder()`, which are removed.
- `Client.v2()`, `Agent.v2()` and `Proxy.v2()` start a draft protocol v2
  connection (`unstable_protocol_v2`), as before.
- `agent_client_protocol::v1` and, with `unstable_protocol_v2`,
  `agent_client_protocol::v2` hold each version's SDK types under the same
  names. Each also re-exports its protocol messages as `schema`:
  `v1::schema` is `schema::v1` and `v2::schema` is `schema::v2`.

The crate root keeps what every version shares: transports (`Channel`,
`Lines`, `ByteStreams`, `Stdio`, `AcpAgent`, `TransportFrame` and the `Raw*`
types), `ConnectTo`, `ConnectionDriver`, the roles, the `JsonRpc*` traits and
derives, `UntypedMessage`, `Error`, `ErrorCode`, `Result`, `Protocol` with
`Raw`/`V1`/`V2`, the generic request handles (`Responder`, `ResponseRouter`,
`SentRequest`, `PreparedRequest`, `Dispatch` and `Builder`, defaulting to
`Raw`), the version-neutral `ConnectionTo`, and the routers that serve both
versions (`AgentProtocolRouter`, `ProxyProtocolRouter`,
`ClientProtocolConnector`). With `unstable_protocol_v2` it also has
`InitializationVersionMismatch` and `SelectedInitializeResponse`, which
`v2::InitializeOutcome::LowerVersion` uses to report a lower version the
peer selected.

| Name | `agent_client_protocol::v1` | `agent_client_protocol::v2` |
| --- | --- | --- |
| `ConnectionTo<Counterpart>` | the v1 connection | the v2 connection |
| `Builder<Host>` | `Builder<Host, .., V1>` | `Builder<Host, .., V2>` |
| `Responder<T>` | `Responder<T, V1>` | `Responder<T, V2>` |
| `ResponseRouter<T>` | `ResponseRouter<T, V1>` | `ResponseRouter<T, V2>` |
| `Dispatch<Req, Notif>` | `Dispatch<Req, Notif, V1>` | `Dispatch<Req, Notif, V2>` |
| `SentRequest<T>`, `PreparedRequest<T>` | `SentRequest<T, V1>`, `PreparedRequest<T, V1>` | `SentRequest<T, V2>`, `PreparedRequest<T, V2>` |
| `SessionBuilder` | new session (`build_session*`) | new session (`build_session*`) |
| `ResumeSessionBuilder` | `resume_session*` | `resume_session*` |
| `SessionSetup` | a session and the response of the request that set it up | a session and the response of the request that set it up |
| `ActiveSession` | the session handle with its update stream | the command-only session handle |
| `ForkSessionBuilder` | - | `fork_session*` (`unstable_session_fork`) |
| `LoadSessionBuilder`, `SessionMessage`, `Blocking`, `NonBlocking`, `SessionBlockState` | v1 session helpers | - |
| `InitializeOutcome` | - | the result of `send_initialize` |
| `schema` | `schema::v1` | `schema::v2` |

### `v1::ConnectionTo` is its own type

`v1::ConnectionTo` is a v1 connection type, like `v2::ConnectionTo`, not the
crate root's version-neutral `ConnectionTo`. It is what callbacks on
`Client.v1()`, `Agent.v1()` and `Proxy.v1()` receive, and it carries the v1
session helpers (`build_session*`, `load_session*`, `resume_session*`).
Requests sent through it return `v1::SentRequest<T>` and
`v1::PreparedRequest<T>`, whose `id()` is a `schema::v1::RequestId`
(`raw_id()` still gives the wire `RawJsonRpcId`). The wire format is
unchanged.

The root `ConnectionTo` stays the raw connection: `HandleDispatchFrom`
handlers, `RunWithConnectionTo` runners, dynamic handlers, `Builder::new`
callbacks and the conductor receive it, and its sends return `Raw` handles.
It no longer has the v1 session helpers. Convert explicitly between the two;
`v2::ConnectionTo` offers the same conversions:

```rust,ignore
use agent_client_protocol::{ConnectionTo, V1, v1};

// raw -> v1, for example in a custom runner that opens a v1 session
let v1_cx: v1::ConnectionTo<Agent> = raw_cx.into_protocol::<V1>()?;
v1_cx.build_session_cwd()?.block_task().start_session().await?;

// v1 -> raw, for example to drive a `RunWithConnectionTo` runner
let raw_cx: ConnectionTo<Agent> = v1_cx.into_raw();
```

Code that spelled out the root type on v1 callbacks or helpers changes the
import:

```rust,ignore
// Before
use agent_client_protocol::{ConnectionTo, SentRequest};
fn send(cx: &ConnectionTo<Agent>) -> SentRequest<PromptResponse> { ... }

// After
use agent_client_protocol::v1::{ConnectionTo, SentRequest};
fn send(cx: &ConnectionTo<Agent>) -> SentRequest<PromptResponse> { ... }
```

Where code compared a v1 request's `id()` with IDs from the raw layer, use
`raw_id()`.

### Converting, spawning and cancelling on connections

- `ConnectionTo::into_protocol::<P>()` returns
  `Result<P::Connection<_>, Error>`. It fails with an Internal error
  (`-32603`) when the connection's builder guards another version, for
  example `into_protocol::<V2>()` on a connection from `Client.v1()`.
  Connections from unguarded builders such as `Builder::new` convert to every
  version. `ConnectionTo::protocol_version()` returns the guarded version, or
  `None` for an unguarded connection.
- `ConnectionTo::spawn_connection` returns the connection type the child
  builder's callbacks receive, like `v1::ConnectionTo::spawn_connection` and
  `v2::ConnectionTo::spawn_connection` already did. It replaces
  `ConnectionTo::spawn_connection_with_context`, which is removed. Spawning
  `Client.v1()` now returns a `v1::ConnectionTo<_>`; call `.into_raw()` where
  the raw connection is wanted. A raw builder still returns the raw
  `ConnectionTo`. Turbofish calls name both parameters:
  `spawn_connection::<UntypedRole, _>(...)`.
- `v1::ConnectionTo::send_cancel_request(_to)` takes
  `impl Into<schema::v1::RequestId>` and `v2::ConnectionTo::send_cancel_request(_to)`
  takes `impl Into<schema::v2::RequestId>`, the type their requests' `id()`
  returns. The v1 change is source-compatible: `RawJsonRpcId` converts into
  `schema::v1::RequestId`, so v1 calls that pass one still compile. V2 calls
  that pass a `RawJsonRpcId` must pass a `schema::v2::RequestId` instead, so
  the v2 methods can no longer send `{"requestId": null}`, which v2 forbids.
  The raw `ConnectionTo` methods still take any `RawJsonRpcId`.
- `Builder::with_v2_protocol_guard` (`unstable_protocol_v2`) is removed. A
  builder that needs the v2 guard is `Client.v2()` or `Agent.v2()`; code that
  needs raw handles in its callbacks converts them with `into_raw()`.

### Every session setup method returns `SessionSetup`

`v1::SessionSetup` and `v2::SessionSetup` replace `RestoredSession` and
`OpenedV2Session`. A `SessionSetup` is the session plus the complete
response of the request that set it up, as public fields. It is
`#[non_exhaustive]`, so destructure it with `..`:

```rust,ignore
#[non_exhaustive]
pub struct SessionSetup<'runner, Link, Response> {   // v2: SessionSetup<Link, Response>
    pub session: ActiveSession<'runner, Link>,
    pub response: Response,
}
```

Every setup method returns one, in both versions: v1 `start_session` for
`session/new` (which used to return a bare `ActiveSession`), `session/load`
and `session/resume`; v2 `session/new`, `session/resume` and
`session/fork`. The v1 `run_until` and `on_session_start` callbacks receive
one too, as do the v2 `on_proxy_session_start` callbacks. The v1
`on_proxy_session_start` callback receives a clone of the complete
`NewSessionResponse` forwarded to the client, and `start_session_proxy`
returns it. Both used to give only the `SessionId`. There is no
`SessionSetup` here, because the session is proxied rather than handed to the
caller.

```rust,ignore
// Before
.on_proxy_session_start(responder, async |session_id| { ... })
let session_id = builder.block_task().start_session_proxy(responder).await?;

// After
.on_proxy_session_start(responder, async |response| { let session_id = response.session_id; ... })
let session_id = builder.block_task().start_session_proxy(responder).await?.session_id;
```

The `RestoredSession` and `OpenedV2Session` accessors are removed in favor of
the fields: `session()`, `session_mut()`, `response()`, `into_parts()` and
`into_session()`. The v1 `ActiveSession` no longer stores the `session/new` response:
`ActiveSession::response()`, `modes()`, `config_options()` and `meta()` are
removed; read them from `SessionSetup::response`.

```rust,ignore
// Before
let mut session = cx.build_session_cwd()?.block_task().start_session().await?;
let response = session.response();
.run_until(async |mut session| { ... })
let (session, response) = restored.into_parts();
let session = opened.into_session();

// After
let SessionSetup { mut session, response, .. } =
    cx.build_session_cwd()?.block_task().start_session().await?;
.run_until(async |SessionSetup { mut session, .. }| { ... })
let SessionSetup { session, response, .. } = restored;
let session = opened.session;
```

### The v1 session stream reports prompt responses and errors

`v1::ActiveSession::send_prompt` used to close the whole connection, and every
session on it, when the agent answered `session/prompt` with an error. The
caller never saw the error. The response, or the error, now arrives on the
session's update stream, and `v1::SessionMessage` changed to carry it:

- `SessionMessage::StopReason(StopReason)` is now
  `SessionMessage::PromptResponse(Result<PromptResponse, Error>)`. It keeps
  the full `PromptResponse`, including `usage` and `_meta`, which used to be
  dropped.
- `SessionMessage::SessionMessage(Dispatch)` is now
  `SessionMessage::Message(v1::Dispatch)`, whose request handles are typed
  for v1. Call `.into_raw()` before handing it to `MatchDispatch`.
- `read_to_string` returns the prompt's error instead of the connection
  closing.
- `send_prompt_blocks(Vec<ContentBlock>)` sends prompts that are not plain
  text.

`v1::SessionBlockState` is sealed. `v1::Blocking` and `v1::NonBlocking` are
its only implementations.

```rust,ignore
// Before
match session.read_update().await? {
    SessionMessage::SessionMessage(dispatch) => MatchDispatch::new(dispatch)
        /* .if_notification(..) */,
    SessionMessage::StopReason(stop_reason) => { /* .. */ }
    _ => {}
}

// After
match session.read_update().await? {
    SessionMessage::Message(dispatch) => MatchDispatch::new(dispatch.into_raw())
        /* .if_notification(..) */,
    SessionMessage::PromptResponse(response) => {
        let stop_reason = response?.stop_reason;
        /* .. */
    }
    _ => {}
}
```

### Renamed and moved items

Every path is relative to `agent_client_protocol`. Items not listed keep
their path.

| Before (3.x) | After |
| --- | --- |
| `Client.builder()` | `Client.v1()` |
| `Agent.builder()` | `Agent.v1()` |
| `Proxy.builder()` | `Proxy.v1()` |
| `Role::builder(role)` (generic code, other roles) | `Builder::new(role)` |
| `Client.connect_with(agent, f)` | `Client.v1().connect_with(agent, f)` |
| `X.builder().without_acp_version_guard()` | `Builder::new(X)` |
| `Builder::without_acp_version_guard()` | removed; `Builder::new` applies no version guard |
| `V2Builder` | `v2::Builder` |
| `V2ConnectionTo` | `v2::ConnectionTo` |
| `V2SessionBuilder` | `v2::SessionBuilder` |
| `V2ResumeSessionBuilder` | `v2::ResumeSessionBuilder` |
| `V2ForkSessionBuilder` | `v2::ForkSessionBuilder` |
| `OpenedV2Session` | `v2::SessionSetup` |
| `V2Session` | `v2::ActiveSession` |
| `InitializeOutcome` | `v2::InitializeOutcome` |
| `InitializationVersionMismatch` | unchanged, at the crate root |
| `InitializationVersionMismatch::selected` (field) | `InitializationVersionMismatch::selected()`, the version of `response` |
| `SelectedInitializeResponse` | unchanged, at the crate root |
| `SessionBuilder` | `v1::SessionBuilder` |
| `ActiveSession` | `v1::ActiveSession` |
| `RestoredSession` | `v1::SessionSetup` |
| `RestoreSessionBuilder` | `v1::LoadSessionBuilder` / `v1::ResumeSessionBuilder` (the generic builder is no longer exported) |
| `SessionMessage` | `v1::SessionMessage` |
| `SessionMessage::SessionMessage(Dispatch)` | `v1::SessionMessage::Message(v1::Dispatch)` |
| `SessionMessage::StopReason(StopReason)` | `v1::SessionMessage::PromptResponse(Result<PromptResponse, Error>)` |
| `Blocking`, `NonBlocking`, `SessionBlockState` | `v1::Blocking`, `v1::NonBlocking`, `v1::SessionBlockState` |
| `schema::InitializeProxyRequest` | `schema::v1::InitializeProxyRequest` |
| `RestoredSession::session()`, `session_mut()`, `response()`, `into_parts()`, `into_session()` (and the `OpenedV2Session` equivalents) | the `session` and `response` fields |
| `ActiveSession::response()`, `modes()`, `config_options()`, `meta()` | removed; use `v1::SessionSetup::response` |
| v1 `SessionBuilder::start_session() -> ActiveSession` | returns `v1::SessionSetup<'static, _, NewSessionResponse>` |
| `ConnectionTo` in v1 callbacks and session helpers | `v1::ConnectionTo` |
| `SentRequest<T>`, `PreparedRequest<T>` from a v1 connection | `v1::SentRequest<T>`, `v1::PreparedRequest<T>` |

`Builder::new`, `Builder::new_with`, `UntypedRole.builder()`,
`Conductor.builder()` and the MCP roles' `builder()` are version-neutral and
stay.

### `Builder::new` applies no version guard

`Builder::new(Client)`, `Builder::new(Agent)` and `Builder::new(Proxy)` (and
`Builder::new_with`) used to apply the v1 wire guard, like
`Client.builder()`. They are now raw: they apply no protocol-version guard,
exactly like `without_acp_version_guard()` did, and their callbacks receive
`Raw` handles. Only `.v1()` applies v1's guard and only `.v2()` applies v2's.
`without_acp_version_guard()` is removed, because no builder needs it any
more.

This changes wire behavior for code that kept `Builder::new` with an ACP
role and relied on the guard. Such a connection no longer:

- negotiates `initialize` (or `proxy/initialize`) down to v1 before the
  handler sees it, so the handler receives the requested version unchanged;
- rejects a peer's response that selects a different version;
- rejects other requests before initialization on an agent that handles
  `initialize`.

Use the versioned entry point to keep the guard:

```rust,ignore
// Before: v1 guard applied implicitly
Builder::new(Agent).on_receive_request(...)

// After
Agent.v1().on_receive_request(...)

// Before: explicitly unguarded
Builder::new(Proxy).without_acp_version_guard()

// After
Builder::new(Proxy)
```

In `agent-client-protocol-test`, Testy follows the same layout:

| Before | After |
| --- | --- |
| `testy::Testy` | `testy::v1::Testy` |
| `testy::V2Testy`, `Testy::new().v2()` | `testy::v2::Testy`, `testy::v2::Testy::new()` |
| `Testy::new().protocol_router()` | `testy::protocol_router()` |

### `v1::schema` and `v2::schema`

Code that imports the schema modules as `v1` and `v2` cannot also import the
SDK modules under those names. Import each version's messages through its SDK
module instead, as `schema`:

```rust,ignore
use agent_client_protocol::v2::{self, schema};

async |request: schema::PromptRequest,
       responder: v2::Responder<schema::PromptResponse>,
       cx: v2::ConnectionTo<Client>| { ... }

// or
use agent_client_protocol::v2::{ConnectionTo, Responder, schema};
```

`agent_client_protocol::schema::v1` and `schema::v2` stay, next to the
version-neutral schema items (`ProtocolVersion`, `SuccessorMessage`, the
`proxy/*` method constants), so code that imports `schema::v2` keeps
compiling.

### A v1 client

```rust,ignore
// Before
use agent_client_protocol::{ActiveSession, Agent, Client, ConnectTo, ConnectionTo};
use agent_client_protocol::schema::{ProtocolVersion, v1::InitializeRequest};

Client
    .builder()
    .connect_with(transport, async |cx: ConnectionTo<Agent>| {
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
        cx.build_session_cwd()?
            .block_task()
            .run_until(async |mut session: ActiveSession<'_, Agent>| {
                session.send_prompt("Hello")?;
                println!("{}", session.read_to_string().await?);
                Ok(())
            })
            .await
    })
    .await

// After
use agent_client_protocol::{Agent, Client, ConnectTo};
use agent_client_protocol::schema::{ProtocolVersion, v1::InitializeRequest};
use agent_client_protocol::v1::{ConnectionTo, SessionSetup};

Client
    .v1()
    .connect_with(transport, async |cx: ConnectionTo<Agent>| {
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
        cx.build_session_cwd()?
            .block_task()
            .run_until(async |SessionSetup { mut session, .. }| {
                session.send_prompt("Hello")?;
                println!("{}", session.read_to_string().await?);
                Ok(())
            })
            .await
    })
    .await
```

### A v1 agent

```rust,ignore
// Before
use agent_client_protocol::{Agent, Responder};
use agent_client_protocol::schema::v1::{InitializeRequest, InitializeResponse};

Agent
    .builder()
    .on_receive_request(
        async |request: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
            responder.respond(InitializeResponse::new(request.protocol_version))
        },
        agent_client_protocol::on_receive_request!(),
    )
    .connect_to(transport)
    .await

// After
use agent_client_protocol::Agent;
use agent_client_protocol::schema::v1::{InitializeRequest, InitializeResponse};
use agent_client_protocol::v1::Responder;

Agent
    .v1()
    .on_receive_request(
        async |request: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
            responder.respond(InitializeResponse::new(request.protocol_version))
        },
        agent_client_protocol::on_receive_request!(),
    )
    .connect_to(transport)
    .await
```

### A v2 client

```rust,ignore
// Before
use agent_client_protocol::{Agent, Client, InitializeOutcome, V2ConnectionTo, V2Session};
use agent_client_protocol::schema::{ProtocolVersion, v2};

Client
    .v2()
    .connect_with(transport, async |cx: V2ConnectionTo<Agent>| {
        let InitializeOutcome::Initialized(_) = cx.send_initialize(request).block_task().await? else {
            return Err(Error::internal_error());
        };
        let session: V2Session<Agent> =
            cx.build_session_cwd()?.start_session().block_task().await?.into_session();
        session.send_prompt("Hello").block_task().await?;
        Ok(())
    })
    .await

// After
use agent_client_protocol::{Agent, Client};
use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::v2::{ConnectionTo, InitializeOutcome, SessionSetup};

Client
    .v2()
    .connect_with(transport, async |cx: ConnectionTo<Agent>| {
        let InitializeOutcome::Initialized(_) = cx.send_initialize(request).block_task().await? else {
            return Err(Error::internal_error());
        };
        let SessionSetup { session, .. } =
            cx.build_session_cwd()?.start_session().block_task().await?;
        session.send_prompt("Hello").block_task().await?;
        Ok(())
    })
    .await
```

### A v2 agent

```rust,ignore
// Before
use agent_client_protocol::{Agent, Client, Responder, V2ConnectionTo};
use agent_client_protocol::schema::v2;

Agent
    .v2()
    .on_receive_request(
        async |request: v2::PromptRequest,
               responder: Responder<v2::PromptResponse>,
               cx: V2ConnectionTo<Client>| { ... },
        agent_client_protocol::on_receive_request!(),
    )
    .connect_to(transport)
    .await

// After
use agent_client_protocol::{Agent, Client};
use agent_client_protocol::schema::v2;
use agent_client_protocol::v2::{ConnectionTo, Responder};

Agent
    .v2()
    .on_receive_request(
        async |request: v2::PromptRequest,
               responder: Responder<v2::PromptResponse>,
               cx: ConnectionTo<Client>| { ... },
        agent_client_protocol::on_receive_request!(),
    )
    .connect_to(transport)
    .await
```

## A version-neutral `Error`

`agent_client_protocol::Error` used to be a re-export of
`schema::v1::Error`, even on v2 connections. It is now the SDK's own type,
the same on every protocol version, with the same three public fields:

```rust,ignore
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub data: Option<serde_json::Value>,
}
```

Errors are sent on the wire exactly as before, on both v1 and v2 connections.

### What still works unchanged

- `Error::new(code, message)`, `.data(...)`, `Error::parse_error()`,
  `invalid_request()`, `method_not_found()`, `invalid_params()`,
  `internal_error()`, `request_cancelled()`, `auth_required()` and
  `Error::into_internal_error(err)`. These codes mean the same in every
  protocol version.
- `agent_client_protocol::Result<T>`, `?` on `serde_json::Error` (becomes
  `-32602`) and on `anyhow::Error` (an `Error`, `schema::v1::Error` or
  `schema::v2::Error` inside it is recovered; anything else becomes `-32603`).
- `Display`, `Debug`, `Clone`, `PartialEq`, `Serialize` and `Deserialize`,
  with the same output as before.

### Version-specific constructors moved to the schema types

`Error::resource_not_found(uri)` and `Error::session_not_found(id)` are
removed from the SDK error, because their data differs between protocol
versions. Build the error for the version you speak and convert it:

```rust,ignore
use agent_client_protocol::{Error, schema::{v1, v2}};

// Before
Err(Error::session_not_found(session_id))
Err(Error::resource_not_found(Some(uri)))

// After, on a v1 connection
Err(v1::Error::session_not_found(session_id).into())
Err(v1::Error::resource_not_found(Some(uri)).into())

// After, on a v2 connection (`unstable_protocol_v2`)
Err(v2::Error::session_not_found(session_id).into())
Err(v2::Error::resource_not_found().into())
```

`agent_client_protocol::util::unsupported_protocol_version` is removed. Use
`v2::Error::unsupported_protocol_version(requested, supported).into()`; the
wire error (`-32022` with `requested` and `supported`) is the same.

### Converting to and from the schema errors

`Error` converts losslessly in both directions with `schema::v1::Error` and,
with `unstable_protocol_v2`, `schema::v2::Error`. Unknown codes and any
`data`, including an explicit `null`, are kept.

```rust,ignore
let error: Error = v1_error.into();
let v2_error = v2::Error::from(error);
```

Typed v2 error data is read with `serde_json` from `error.data`:

```rust,ignore
if error.code == v2::ErrorCode::ResourceNotFound
    && let Some(data) = error.data.clone()
    && let Ok(data) = serde_json::from_value::<v2::SessionNotFoundErrorData>(data)
{
    // data.session_id
}
```

### `ErrorCode` is the SDK's own

`agent_client_protocol::ErrorCode` used to be `schema::v1::ErrorCode`. It is
now a version-neutral newtype over the `i32` on the wire, with constants for
the codes every version defines the same way:

| Before (`v1::ErrorCode` variant) | After |
| --- | --- |
| `ErrorCode::ParseError` | `ErrorCode::PARSE_ERROR` |
| `ErrorCode::InvalidRequest` | `ErrorCode::INVALID_REQUEST` |
| `ErrorCode::MethodNotFound` | `ErrorCode::METHOD_NOT_FOUND` |
| `ErrorCode::InvalidParams` | `ErrorCode::INVALID_PARAMS` |
| `ErrorCode::InternalError` | `ErrorCode::INTERNAL_ERROR` |
| `ErrorCode::RequestCancelled` | `ErrorCode::REQUEST_CANCELLED` |
| `ErrorCode::AuthRequired` | `ErrorCode::AUTH_REQUIRED` |
| `ErrorCode::ResourceNotFound` | `ErrorCode::RESOURCE_NOT_FOUND` |
| `ErrorCode::Other(code)` | `ErrorCode::new(code)` or `code.into()` |

`ErrorCode` compares directly with `schema::v1::ErrorCode` and
`schema::v2::ErrorCode`, so you can also keep the schema enum and name the
version:

```rust,ignore
assert_eq!(error.code, v1::ErrorCode::InvalidParams);
assert_eq!(error.code, v2::ErrorCode::UnsupportedProtocolVersion);
```

Codes defined by only some versions, such as v2's `-32022`, have no
`ErrorCode` constant; compare with that version's enum. `i32::from(code)`
and `ErrorCode::value()` give the raw code.

### Code that used `schema::v1::Error` as the SDK error

Because the two used to be the same type, some code imported `Error` from
`schema::v1` and used it with SDK callbacks. Import
`agent_client_protocol::Error` instead, or convert with `.into()`.

## A version-neutral `RawJsonRpcId`

The raw JSON-RPC layer used `schema::v1::RequestId` as its ID type, even on
v2 connections. It now uses the SDK's own `agent_client_protocol::RawJsonRpcId`,
the same on every protocol version:

```rust,ignore
pub enum RawJsonRpcId {
    Null,
    Number(i64),
    Str(String),
}
```

It has the same variants, serialization, `Display`, `Hash` and ordering as
`schema::v1::RequestId`, so IDs are sent on the wire exactly as before: `1`
and `"1"` stay distinct, and integers keep the full 64-bit signed range.

### What changed

| Before | After |
| --- | --- |
| `RawJsonRpcMessage::Request(schema::v1::Request<RawJsonRpcParams>)` | `RawJsonRpcMessage::Request(RawJsonRpcRequest)`, with the same `id`, `method` and `params` fields |
| `RawJsonRpcMessage::Notification(schema::v1::Notification<RawJsonRpcParams>)` | `RawJsonRpcMessage::Notification(RawJsonRpcNotification)`, with the same `method` and `params` fields |
| `RawJsonRpcResponse`, an alias of `schema::v1::Response<Value, Box<RawJsonRpcError>>` | `RawJsonRpcResponse`, the SDK's own enum with the same `Result { id, result }` and `Error { id, error }` variants and `new` |
| `RawJsonRpcMessage::response_id()` returned `Option<&v1::RequestId>` | returns `Option<&RawJsonRpcId>` |

The request handles keep a typed `id()`; see
[Request handles are typed by protocol version](#request-handles-are-typed-by-protocol-version).

`RawJsonRpcMessage::request`, `RawJsonRpcMessage::response` and
`RawJsonRpcResponse::new` take `impl Into<RawJsonRpcId>`, so passing a
`v1::RequestId`, `v2::RequestId`, `i64` or `String` still compiles. So do
`ConnectionTo::send_cancel_request` and `send_cancel_request_to`. A bare
`1.into()` no longer infers its type; write `RawJsonRpcId::Number(1)`.

### Updating your code

Replace `schema::v1::RequestId` with `RawJsonRpcId` where you build or match
raw messages, or store IDs from the raw dispatch layer or from several protocol
versions:

```rust,ignore
use agent_client_protocol::{RawJsonRpcId, RawJsonRpcMessage};

// Before
let request = RawJsonRpcMessage::request(method, params, v1::RequestId::Number(1))?;
let mut pending: HashMap<v1::RequestId, Route> = HashMap::new();

// After
let request = RawJsonRpcMessage::request(method, params, RawJsonRpcId::Number(1))?;
let mut pending: HashMap<RawJsonRpcId, Route> = HashMap::new();
pending.insert(responder.raw_id().clone(), route);
```

Where a schema type needs a request ID, such as `v1::CancelRequestNotification`
or an elicitation's `requestId`, convert. `RawJsonRpcId` converts losslessly to
and from `v1::RequestId`, and from `v2::RequestId`. Converting to
`v2::RequestId` fails only for `null`, with the same Invalid Request error
(`-32600`) a v2 connection sends for a `null` request ID:

```rust,ignore
let raw_id = RawJsonRpcId::from(cancel.request_id);
let v1_id = v1::RequestId::from(raw_id.clone());
let v2_id = v2::RequestId::try_from(raw_id)?;
```

## Request handles are typed by protocol version

`Responder`, `ResponseRouter`, `SentRequest`, `PreparedRequest`, `Dispatch`
and `Builder` take a protocol marker as their last type parameter. The
markers implement the sealed `agent_client_protocol::Protocol` trait, and the
marker selects the type that `id()` returns:

| Marker | Used by | `id()` |
| --- | --- | --- |
| `Raw` (the default) | `HandleDispatchFrom`, `MatchDispatch`, `ConnectionTo::send_request`, `Builder::new` and other unguarded builders | `&RawJsonRpcId` |
| `V1` | typed handlers on `Client.v1()`, `Agent.v1()`, `Proxy.v1()`; `v1::ConnectionTo` | `&schema::v1::RequestId` |
| `V2` (`unstable_protocol_v2`) | typed handlers on `Client.v2()`, `Agent.v2()`, `Proxy.v2()`; `v2::ConnectionTo` | `&schema::v2::RequestId` |

`Dispatch::id()` returns an `Option` of the same type. A `V2` `id()` never
fails: the SDK rejects `null` request IDs on v2 connections and never assigns
`null` itself.

Every handle also has `raw_id()`, which returns the ID as a `&RawJsonRpcId`
whatever the marker (`Option<&RawJsonRpcId>` on `Dispatch`).

### The raw dispatch layer is version-neutral

`HandleDispatchFrom`, `MatchDispatch`/`MatchDispatchFrom`, dynamic handlers,
`Role::default_handle_dispatch_from` and requests sent through `ConnectionTo`
are not tied to one protocol version. Their handles use the `Raw` marker,
which is the default, so `Dispatch`, `Responder<T>` and `SentRequest<T>`
written without a marker are the raw ones, and their `id()` returns
`&RawJsonRpcId`:

```rust,ignore
// Before (3.x): id() was a v1::RequestId even on v2 connections
impl HandleDispatchFrom<Client> for MyRouter {
    async fn handle_dispatch_from(&mut self, message: Dispatch, cx: ConnectionTo<Client>)
        -> Result<Handled<Dispatch>, Error>
    {
        let id: Option<&v1::RequestId> = message.id();
        ...
    }
}

// After: the signature is unchanged, and id() is the wire ID
        let id: Option<&RawJsonRpcId> = message.id();
```

Code that stored these IDs as `v1::RequestId` switches to `RawJsonRpcId`, or
converts with `v1::RequestId::from(id.clone())`.

### Typed handlers on v1 builders

Typed handlers on `Client.v1()`, `Agent.v1()` and `Proxy.v1()` receive `V1`
handles, so `id()` still returns `&schema::v1::RequestId`. Closures whose
parameters are not annotated need no change. Where the handle type is spelled
out, use the `v1` alias (or add the marker), because the root default is now
`Raw`:

```rust,ignore
use agent_client_protocol::v1::{Dispatch, Responder};

// Before
Agent.builder().on_receive_request(
    async |request: PromptRequest, responder: Responder<PromptResponse>, cx| { ... },
    on_receive_request!(),
)
// After
Agent.v1().on_receive_request(
    async |request: PromptRequest, responder: Responder<PromptResponse>, cx| { ... },
    on_receive_request!(),
)

// Likewise for on_receive_dispatch
async |message: Dispatch<PromptRequest, SessionNotification>, cx| { ... }
```

`Client.v1()`, `Agent.v1()` and `Proxy.v1()` return `v1::Builder<_>`
(`Builder<_, _, _, _, V1>`).
`Builder::new` and other roles' builders use `Raw`. The v1 session helpers that take
a responder, such as `v1::SessionBuilder::on_proxy_session_start`, take a
`v1::Responder<_>`.

### v2 code

`id()` on handles from the v2 builders and `v2::ConnectionTo` returns a
`&schema::v2::RequestId` instead of a `&schema::v1::RequestId`:

```rust,ignore
// Before
let id: &v1::RequestId = responder.id();

// After
let id: &v2::RequestId = responder.id();
let raw: &RawJsonRpcId = responder.raw_id();
```

Closures registered on a v2 builder whose parameters are not annotated need
no change. Where a v2 handle type is spelled out, use the `v2` alias:

```rust,ignore
use agent_client_protocol::v2::{ActiveSession, Responder, SentRequest};

// Before
async |request: v2::PromptRequest, responder: Responder<v2::PromptResponse>, cx| { ... }
fn send(session: &V2Session<Agent>) -> SentRequest<v2::PromptResponse> { ... }

// After
async |request: v2::PromptRequest, responder: Responder<v2::PromptResponse>, cx| { ... }
fn send(session: &ActiveSession<Agent>) -> SentRequest<v2::PromptResponse> { ... }
```

### Getting a typed handle from the raw layer

`Responder`, `ResponseRouter`, `SentRequest`, `PreparedRequest` and
`Dispatch` with the `Raw` marker convert to a typed marker with
`into_protocol`. Only the ID's type changes. It fails
only for a `null` ID on `V2`, with the Invalid Request error (`-32600`) a v2
connection sends for such a request; a request received on a v2 connection
never has one. Any handle converts back with the infallible `into_raw`:

```rust,ignore
MatchDispatchFrom::new(message, &cx)
    .if_request_from(Client, async |request: v2::NewSessionRequest, responder| {
        // `responder` is a `Responder<_, Raw>`; the v2 helper wants `V2`.
        let responder = responder.into_protocol::<V2>()?;
        v2_cx.build_session_from(request).on_proxy_session_start(responder, op)
    })
    .await
```

Helpers that accept handles of several versions can take the marker as a
type parameter:

```rust,ignore
fn respond<P: agent_client_protocol::Protocol>(responder: Responder<MyResponse, P>) { ... }
```

### Forwarding between versions

`SentRequest::forward_response_to`, `on_receiving_ok_result` (and the
`PreparedRequest` equivalents) accept a responder of any marker, and
`send_proxied_message` / `send_proxied_message_to` accept a `Dispatch` of any
marker, so forwarding between connections needs no conversion.

### `MatchDispatch::otherwise_ignore` answers unmatched requests

`MatchDispatch::otherwise_ignore` now answers an unmatched request with a
method not found error (`-32601`). Before, it dropped the responder, so the
peer got no answer. Unmatched notifications and
responses are still ignored.

`v1::ActiveSession::read_to_string` uses `otherwise_ignore`, so a request routed
to the session that it does not handle, such as `session/request_permission`,
now gets `-32601`. Handle such requests yourself if the peer needs a different
answer.

### Renamed builder context types

The hidden `ConnectionContext` trait and its `RawConnectionContext` and
`V2ConnectionContext` implementations are replaced by `Protocol`, `Raw`, `V1`
and `V2`. `Protocol::Connection<Counterpart>` is the connection type builder
callbacks receive (`ConnectionTo` for `Raw`, `v1::ConnectionTo` for `V1`,
`v2::ConnectionTo` for `V2`), so code generic over a builder's context changes
`Context: ConnectionContext` to `P: Protocol`.

Generic code can turn a `P::Connection<Counterpart>` back into the
version-neutral `ConnectionTo` with `.into()`: `v1::ConnectionTo` and
`v2::ConnectionTo` implement `From` for it, and `Protocol::Connection`
requires `Into<ConnectionTo<Counterpart>>`. Their own methods, such as the v1
session helpers, are only available on the concrete types.

## `Channel`, `JsonRpcMessage::parse_message` and `JsonRpcResponse::from_json`

- `Channel` is `#[non_exhaustive]`. Build one with `Channel::new(rx, tx)`
  (or `Channel::duplex()`) and take it apart with `into_parts()` or a pattern
  with `..`:

  ```rust,ignore
  // Before
  let channel = Channel { rx, tx };
  let Channel { rx, tx } = channel;

  // After
  let channel = Channel::new(rx, tx);
  let Channel { rx, tx, .. } = channel;
  ```

- `JsonRpcMessage::parse_message` takes `params: &serde_json::Value` instead
  of `&impl Serialize`. Update hand-written implementations; derived ones
  change with the derive macros. Callers holding raw
  `Option<RawJsonRpcParams>` pass
  `&params.map_or(serde_json::Value::Null, RawJsonRpcParams::into_value)`.
- `JsonRpcResponse::from_value` is renamed to `from_json`, to pair with
  `into_json`. Rename it in hand-written implementations and calls such as
  `PromptResponse::from_value(method, value)`; derived implementations change
  with the derive macros.

## Core changes

### JSON-RPC batches are rejected

`TransportBatch`, `TransportBatchEntry` and `TransportFrame::Batch` are
removed. The SDK no longer processes JSON-RPC batches. A batch (a JSON array
on the wire, including an empty one) gets one Invalid Request error
(`-32600`) with `"id": null`, and none of its entries run. Send each message
on its own. Code that built or matched `TransportFrame::Batch` removes that
case.

`TransportFrame` is `#[non_exhaustive]`, so matches on it need a wildcard arm:

```rust,ignore
match frame {
    TransportFrame::Single(message) => { /* .. */ }
    TransportFrame::Malformed { raw, error } => { /* .. */ }
    _ => {}
}
```

### `MetaCapability` and `MetaCapabilityExt` are removed

The `MetaCapability` and `MetaCapabilityExt` traits, which read and wrote
capability flags under `_meta.symposium` in `initialize` requests and
responses, are removed. Read and write the `_meta` map of the
`InitializeRequest` or `InitializeResponse` directly if you still need those
flags.

### A dropped `Responder` answers the request

When a `Responder` is dropped without a response, the SDK now answers the
request itself:

- `-32800` (`Error::request_cancelled`) if the peer cancelled the request with
  `$/cancel_request`;
- otherwise `-32603` (`Error::internal_error`), with data naming the method.

Before, the peer got no answer and waited. This change compiles without any
warning. Check handlers that drop a responder on purpose, for example to
answer later through another path: move the responder to the code that
answers instead of dropping it. Handing it to something that answers through
it, such as `SentRequest::forward_response_to`, or moving it into another task
does not trigger the automatic answer. No automatic answer is sent while the
connection shuts down.

### v1 forwarding keeps unknown fields

v1 proxies and conductor chains no longer parse and re-serialize the messages
they forward. Fields the SDK does not know, for example in `session/new`
requests and responses or in `initialize`, now reach the next component
instead of being dropped. Proxies that relied on forwarding to strip such
fields must remove them themselves.

### `ProxyProtocolRouter` keeps reading after a rejected first message

`ProxyProtocolRouter` (`unstable_protocol_v2`) used to close the connection
when it rejected the first `proxy/initialize`, for example for an unsupported
version, or when the first frame was malformed. It now answers the rejected
request or malformed frame with an error and keeps reading, like
`AgentProtocolRouter`, so the conductor can send `proxy/initialize` again. The
router finishes when its input closes.

## Schema enums and SDK helpers

### v1 extension methods keep their `_` prefix

Parsing a v1 dispatch enum (`schema::v1::ClientRequest`, `AgentRequest`,
`ClientNotification` or `AgentNotification`) from an extension method used to
strip the leading `_`: a parsed `_vendor/x` reported `method()` as `vendor/x`
and was sent on as `vendor/x`, which the peer answered with Method not found.
The `ExtMethodRequest` and `ExtNotification` variants now keep the method
exactly as it was on the wire, as v2 already did. Code that matched on
`ext.method` without the prefix matches the full name instead:

```rust,ignore
// Before
v1::ClientRequest::ExtMethodRequest(ext) if &*ext.method == "vendor/x" => ...
// After
v1::ClientRequest::ExtMethodRequest(ext) if &*ext.method == "_vendor/x" => ...
```

### Removed `util` helpers

`util::internal_error(message)` and `util::parse_error(message)` duplicated the
`Error` constructors and are removed. Build the error directly:

```rust,ignore
// Before
Err(util::internal_error(format!("lost {id}")))
result.map_err(util::internal_error)?;

// After
Err(Error::internal_error().data(format!("lost {id}")))
result.map_err(Error::into_internal_error)?; // any `std::error::Error`
```

`util::TypeNotification` is removed. It duplicated
`MatchDispatch::if_notification` for notifications. Wrap the message in
`Dispatch::Notification` and use `MatchDispatch`, which returns a parse error
for malformed params instead of logging and dropping them:

```rust,ignore
// Before
TypeNotification::new(message)
    .handle_if(async |notif: SessionNotification| { ... })
    .await
    .otherwise(async |untyped| { ... })
    .await

// After
MatchDispatch::new(Dispatch::Notification(message))
    .if_notification(async |notif: SessionNotification| { ... })
    .await
    .otherwise(async |dispatch| { ... })
    .await
```

`agent-client-protocol` now depends on the `agent-client-protocol-derive`
version released with it exactly (`=`), because the derive output calls
hidden SDK internals. Depend on the derives through `agent_client_protocol`
rather than adding the derive crate separately.

### MCP-over-ACP error codes are `ErrorCode`s

`mcp_server::MCP_SERVER_UNAVAILABLE` (`-33001`) and
`mcp_server::MCP_BACKEND_FAILURE` (`-33002`) are now `ErrorCode` constants
instead of `i32`, and `mcp_server::MCP_RESOURCE_LIMIT_EXCEEDED` (`-33000`) is
new. `Error::new(MCP_BACKEND_FAILURE, ..)` still compiles. Compare with
`error.code == MCP_BACKEND_FAILURE` directly instead of through
`i32::from(error.code)`, and use `.value()` where you need an `i32` (for
example, against a JSON `code`).

## Proxy method names

The proxy protocol now uses the method names defined by the
[ACP proxy chains RFD](https://agentclientprotocol.com/rfds/proxy-chains).
Earlier SDK releases used `_`-prefixed names, which ACP reserves for vendor
extensions.

| Old method | New method |
| --- | --- |
| `_proxy/initialize` | `proxy/initialize` |
| `_proxy/successor` | `proxy/successor` |

The message shapes are unchanged. `proxy/successor` still flattens the inner
ACP message into its params and sends optional metadata as `_meta`. The
non-spec `meta` key is no longer read on receive; see
[`SuccessorMessage` and `InitializeProxyRequest` are non-exhaustive](#successormessage-and-initializeproxyrequest-are-non-exhaustive).

### Upgrade conductors and proxies together

This is a wire-breaking rename with no compatibility fallback: the SDK no
longer sends or accepts the old `_proxy/*` names. A conductor built with this
release cannot drive a proxy built with an earlier release, and vice versa.
Upgrade the conductor and every proxy in a chain at the same time, including
nested conductors running in proxy mode.

Code that uses the SDK types (`schema::v1::InitializeProxyRequest`, formerly
`schema::InitializeProxyRequest`, `schema::v2::InitializeProxyRequest`,
`SuccessorMessage`, `METHOD_INITIALIZE_PROXY`, `METHOD_SUCCESSOR_MESSAGE`, or
the `Proxy` and `Conductor` roles) needs no changes for the rename. Update any hard-coded method
strings, such as in raw JSON-RPC handlers, test fixtures, or trace filters.

### `SuccessorMessage` and `InitializeProxyRequest` are non-exhaustive

`schema::SuccessorMessage`, `schema::v1::InitializeProxyRequest` and
`schema::v2::InitializeProxyRequest` are `#[non_exhaustive]`, so the proxy
protocol can add fields in a minor release. Their fields stay public for
reading, but outside the SDK you build them with constructors:

```rust,ignore
// Before
let message = SuccessorMessage { message: request, meta: None };
let init = v1::InitializeProxyRequest { initialize };

// After
let message = SuccessorMessage::new(request);
let message = SuccessorMessage::new(request).meta(meta);
let init = v1::InitializeProxyRequest::new(initialize); // or initialize.into()
```

`SuccessorMessage::meta` is now `Option<serde_json::Map<String, Value>>`
instead of `Option<Value>`, because ACP `_meta` is always an object. A
`proxy/successor` request whose `_meta` is not an object is accepted and
its `_meta` is ignored, and the non-spec `meta` key is ignored instead of
being read as `_meta`.

### `ConductorHostRole` is sealed

`agent-client-protocol-conductor::ConductorHostRole` can no longer be
implemented outside the conductor crate. Its methods take a `ConductorRunner`,
whose fields are private, so no external implementation could be written
before either. Use the provided `Agent` and `Proxy` host roles.

`ConductorHostRole` is now a marker trait: its `Instantiator` type and its
`initialize_method`, `select_protocol_version`, `instantiate` and
`handle_dispatch` methods moved to a private supertrait and take an argument
only the conductor crate can create, so code outside it cannot call them.
`ConductorImpl::new`
is private. Create conductors with `ConductorImpl::new_agent` or
`ConductorImpl::new_proxy`.

## Conductor changes

### Instantiators receive a `SelectedInitializeRequest`

`InstantiateProxies::instantiate_proxies`,
`InstantiateProxiesAndAgent::instantiate_proxies_and_agent` and instantiator
closures receive a `SelectedInitializeRequest` instead of a
`schema::v1::InitializeRequest`, and return one inside `InstantiatedProxies`
or `InstantiatedChain`. It is the client's `initialize` request, parsed for
the protocol version the conductor selected: `SelectedInitializeRequest::V1`
or, with `unstable_protocol_v2`, `SelectedInitializeRequest::V2`. This
affects every custom instantiator, including v1-only ones.

A v1-only instantiator converts the request with `into_v1()?`:

```rust,ignore
use agent_client_protocol_conductor::{
    ConductorImpl, InstantiatedChain, SelectedInitializeRequest,
};

// Before
ConductorImpl::new_agent("my-conductor", |init_req: InitializeRequest| async move {
    Ok((init_req, proxies, agent))
})

// After
ConductorImpl::new_agent("my-conductor", |init_req: SelectedInitializeRequest| async move {
    let init_req = init_req.into_v1()?;
    Ok(InstantiatedChain::new(init_req, proxies, agent))
})
```

`SelectedInitializeRequest` is `#[non_exhaustive]`, so an instantiator that
handles each version matches the variants and adds a wildcard arm:

```rust,ignore
match init_req {
    SelectedInitializeRequest::V1(request) => { /* .. */ }
    SelectedInitializeRequest::V2(request) => { /* .. */ }
    _ => return Err(Error::invalid_request()),
}
```

### The v2 instantiator methods are removed

`InstantiateProxies::instantiate_v2_proxies` and
`InstantiateProxiesAndAgent::instantiate_v2_proxies_and_agent`
(`unstable_protocol_v2`) are removed. The single method receives a v2 request
as `SelectedInitializeRequest::V2`. Move the body of a v2 method into the
`V2` arm of a match.

### `ConductorHostRole::initialize` and `InitializationOutcome` are removed

`ConductorHostRole::initialize`, `ConductorHostRole::initialize_with_outcome`
and `InitializationOutcome` (`unstable_protocol_v2`) are removed. The
conductor initializes the chain itself and only calls the instantiator. See
[`ConductorHostRole` is sealed](#conductorhostrole-is-sealed).

### Conductor internals are no longer exported

The conductor crate exports its API explicitly instead of through a glob.
`ConductorRunner`, `ConductorMessage`, `ConductorSuccessor`, `ComponentIndex`
and `SourceComponentIndex` are no longer public; they only described the
conductor's internal routing.

### Instantiators return `InstantiatedChain` and `InstantiatedProxies`

`InstantiateProxiesAndAgent` and `InstantiateProxies`, including instantiator
closures, return `#[non_exhaustive]` structs instead of tuples.

| Before | After |
| --- | --- |
| `Ok((request, proxies, agent))` | `Ok(InstantiatedChain::new(request, proxies, agent))` |
| `Ok((request, proxies))` | `Ok(InstantiatedProxies::new(request, proxies))` |

`new` takes `impl Into<SelectedInitializeRequest>`, so a `v1::InitializeRequest`
needs no `.into()`.

### Instantiators can negotiate down

`SelectedInitializeRequest::into_v1` now converts a v2 request to v1 the way
the SDK's agents negotiate `initialize`, instead of failing. An instantiator
may return a request for a lower version than the one the conductor selected,
and the chain then initializes with that version. A v1-only instantiator that
calls `into_v1()?` therefore accepts a v2 client and answers it with a v1
`initialize` response, where it used to reject it with `-32600`. Returning a
request for a version above the selected one is still rejected.

### Other conductor changes

- `ConductorArgs`, `ConductorCommand` and its variants are `#[non_exhaustive]`.
  Create `ConductorArgs` by parsing arguments with `clap::Parser`.
- `trace::Protocol` is renamed to `trace::MessageProtocol`, so it no longer
  shares a name with the core `Protocol` trait. The serialized trace format is
  unchanged.
- The conductor's `unstable_protocol_v2` feature no longer enables the
  polyfill's feature (it only did so for the conductor's own tests). Enable
  `unstable_protocol_v2` on `agent-client-protocol-polyfill` yourself for a v2
  chain.
- `McpOverAcpPolyfill` is `#[non_exhaustive]`. Create it with
  `McpOverAcpPolyfill::http()`.
- The polyfill rewrites `McpServer::Acp` entries in `session/fork` in every
  build. It used to forward them unchanged unless `unstable_session_fork` was
  enabled.
- The polyfill forwards a repeated `proxy/initialize` on the connection's
  protocol version instead of closing its connection. An invalid
  `protocolVersion` is answered with `-32602` instead of `-32603`, and a
  request for a different version than the first one with `-32600`.
- Conductor traces record the messages between the last proxy and the agent.
  Trace snapshots and tools that read traces see these extra events.
- `agent_client_protocol_trace_viewer::TraceViewerConfig` is
  `#[non_exhaustive]`. Start from `TraceViewerConfig::default()` and use the
  `port` and `open_browser` setters. `TraceSource` is no longer public.

## The rmcp integration moves behind the `rmcp_3` feature

`agent-client-protocol-rmcp` now has one module per supported rmcp major
version, each behind a matching Cargo feature. The rmcp 3.x integration lives
in the `rmcp_3` module, enabled by the `rmcp_3` feature. No rmcp feature is
enabled by default, so without one the crate is empty.

Enable the feature next to the features you already use; your own `rmcp`
dependency stays as it is:

```toml
[dependencies]
agent-client-protocol-rmcp = { version = "5", features = ["rmcp_3", "unstable_mcp_over_acp"] }
rmcp = "3"
```

The `unstable_mcp_over_acp` passthrough feature is unchanged and applies to
every rmcp module. The `unstable_protocol_v2` passthrough feature is removed
because the crate has no v2-specific code. Enable `unstable_protocol_v2` on
`agent-client-protocol` instead.

Then add `rmcp_3::` to every import from the crate:

| Before | After |
| --- | --- |
| `agent_client_protocol_rmcp::McpServerExt` | `agent_client_protocol_rmcp::rmcp_3::McpServerExt` |
| `agent_client_protocol_rmcp::McpServerBuilder` | `agent_client_protocol_rmcp::rmcp_3::McpServerBuilder` |
| `agent_client_protocol_rmcp::McpTool` | `agent_client_protocol_rmcp::rmcp_3::McpTool` |
| `agent_client_protocol_rmcp::tool_fn!` | `agent_client_protocol_rmcp::rmcp_3::tool_fn!` |
| `agent_client_protocol_rmcp::tool_fn_mut!` | `agent_client_protocol_rmcp::rmcp_3::tool_fn_mut!` |

`McpTool`, `tool_fn!` and `tool_fn_mut!` are re-exports of the
core crate's items, which you can also import from
`agent_client_protocol::mcp_server` and `agent_client_protocol` directly.

Future rmcp major versions arrive as new features and modules (such as
`rmcp_4`) in minor releases, so you can enable both while moving your services
to the new rmcp major version. Removing support for an rmcp major version is a
breaking change and only happens in an ACP major release.

### Other rmcp and `mcp_server` changes

- `EnabledTools` is no longer public. No public API took or returned it. Use
  `enable_tool`, `disable_tool`, `enable_all_tools`, and `disable_all_tools`.
- `McpServerBuilder::enable_tool` / `disable_tool` and
  `McpToolRegistry::enable_tool` / `disable_tool` return `UnknownToolError`
  instead of a `-32600` `Error`. `?` still converts it into an `Error`.
- `McpServerExt` is sealed. It was only meant to be implemented for
  `McpServer`.
- `McpConnectionContext::Acp` is `#[non_exhaustive]`. Match it as
  `McpConnectionContext::Acp { server_id, .. }`.
- Builder-built servers report the builder name and `McpServerBuilder::version`
  (default `0.0.0`) as their MCP `serverInfo`, not `rmcp`.
- An error from a builder tool, including arguments that do not match its input
  schema, is a `tools/call` result with `isError: true` instead of a JSON-RPC
  error. A call to an unknown or disabled tool still gets `-32602`.
  `RegisteredMcpTool::call_tool` reports bad arguments as `-32602` instead of
  `-32603`.
- The native MCP-over-ACP path for `McpServer::from_rmcp` and builder servers
  follows the core crate's `unstable_mcp_over_acp` feature. Before, enabling
  the feature only on `agent-client-protocol` made each request bridge a new
  rmcp service.

## HTTP transport

`agent-client-protocol-http` narrows its public types:

- `HttpClientError` is `#[non_exhaustive]`, so matches need a wildcard arm.
  `InvalidUrl` and `Reqwest` no longer implement `From`, and their `Display`
  text no longer repeats the underlying error. Walk
  `std::error::Error::source` to see it. `InvalidUrl` now holds an opaque
  `InvalidUrlError` instead of `url::ParseError`; it displays the parser's
  message.
- `CorsOptions` is an opaque struct. The `Disabled`, `AllowOrigins`, and
  `AllowAnyOrigin` variants are gone. Use `CorsOptions::disabled()`,
  `CorsOptions::allow_origins(...)`, and `CorsOptions::allow_any_origin()`, and
  compare policies with `==`.
- `CorsOptions::allow_origins` returns `InvalidOriginError` instead of
  `axum::http::header::InvalidHeaderValue`. It names the rejected origin
  through `origin()`.

The crate does not re-export `axum`, `reqwest`, or `url`. Its public API uses
third-party types in four places: `AcpHttpServer::into_router` returns an
`axum::Router`, `HttpClientBuilder::configure_http` and
`HttpClient::from_http_client` take reqwest types, and
`HttpClientError::Reqwest` holds a `reqwest::Error`. Depend on matching
versions of those crates to use them.

## `AcpAgent`

- `AcpAgent::spawn_process` is no longer public. It returned `async_process`
  types and skipped the process-group teardown that `ConnectTo` installs.
  Connect the `AcpAgent` itself instead.
- `AcpAgent::claude_agent()` and `AcpAgent::codex()` are removed. They ran
  `npx -y ...@latest`, which installs whatever version was published last.
  Name the adapter version yourself:

```rust
use std::str::FromStr;
use agent_client_protocol::AcpAgent;

let agent = AcpAgent::from_str("npx -y @agentclientprotocol/claude-agent-acp@<version>")?;
# Ok::<(), agent_client_protocol::Error>(())
```
