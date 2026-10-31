# Migrating to v5

This page covers the breaking changes in `agent-client-protocol` 5.0 and the
crates released with it.

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
`ClientProtocolConnector`).

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
| `LoadSessionBuilder`, `RestoreSessionBuilder`, `SessionMessage`, `Blocking`, `NonBlocking`, `SessionBlockState` | v1 session helpers | - |
| `InitializeOutcome`, `InitializationVersionMismatch`, `SelectedInitializeResponse` | - | the result of `send_initialize` |
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
let v1_cx: v1::ConnectionTo<Agent> = raw_cx.into_protocol::<V1>();
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

### Every session setup method returns `SessionSetup`

`v1::SessionSetup` and `v2::SessionSetup` replace `v1::OpenedSession`
(formerly `RestoredSession`) and `v2::OpenedSession` (formerly
`OpenedV2Session`). A `SessionSetup` is the session plus the complete
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
`on_proxy_session_start` callback still receives the `SessionId`, because the
session is proxied rather than handed to the caller.

The accessors are removed in favor of the fields: `session()`,
`session_mut()`, `response()`, `into_parts()` and `into_session()`. The v1
`ActiveSession` no longer stores the `session/new` response:
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
| `InitializationVersionMismatch` | `v2::InitializationVersionMismatch` |
| `SelectedInitializeResponse` | `v2::SelectedInitializeResponse` |
| `SessionBuilder` | `v1::SessionBuilder` |
| `ActiveSession` | `v1::ActiveSession` |
| `RestoredSession` | `v1::SessionSetup` |
| `RestoreSessionBuilder` | `v1::RestoreSessionBuilder` (also `v1::LoadSessionBuilder` / `v1::ResumeSessionBuilder` for each request) |
| `SessionMessage` | `v1::SessionMessage` |
| `Blocking`, `NonBlocking`, `SessionBlockState` | `v1::Blocking`, `v1::NonBlocking`, `v1::SessionBlockState` |
| `schema::InitializeProxyRequest` | `schema::v1::InitializeProxyRequest` |
| `OpenedSession` (`v1` and `v2`) | `v1::SessionSetup`, `v2::SessionSetup` (public `session` and `response` fields) |
| `OpenedSession::session()`, `session_mut()`, `response()`, `into_parts()`, `into_session()` | the `session` and `response` fields |
| `v1::ActiveSession::response()`, `modes()`, `config_options()`, `meta()` | removed; use `SessionSetup::response` |
| v1 `SessionBuilder::start_session() -> ActiveSession` | returns `v1::SessionSetup<'static, _, NewSessionResponse>` |
| `Responder<T, V1>` (and the other handles) | unchanged, or `v1::Responder<T>` |
| `ConnectionTo` in v1 callbacks and session helpers | `v1::ConnectionTo` |
| `SentRequest<T>`, `PreparedRequest<T>` from a v1 connection | `v1::SentRequest<T>`, `v1::PreparedRequest<T>` |
| `Responder<T, V2>` (and the other handles) | unchanged, or `v2::Responder<T>` |

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
`Builder::new`, other roles' builders, and `with_v2_protocol_guard()`
(which returns a `Raw` builder) use `Raw`. The v1 session helpers that take
a responder, such as `v1::SessionBuilder::on_proxy_session_start`, take a
`v1::Responder<_>`.

### v2 code

The `v2_id()` accessors are removed. `id()` on handles from the v2 builders and
`v2::ConnectionTo` returns the v2 ID directly:

```rust,ignore
// Before
let id: Option<v2::RequestId> = responder.v2_id();
let id: &v2::RequestId = sent.v2_id();

// After
let id: &v2::RequestId = responder.id();
let id: &v2::RequestId = sent.id();
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

`Responder`, `ResponseRouter` and `Dispatch` with the `Raw` marker convert
to a typed marker with `into_protocol`. Only the ID's type changes. It fails
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

### Renamed builder context types

The hidden `ConnectionContext` trait and its `RawConnectionContext` and
`V2ConnectionContext` implementations are replaced by `Protocol`, `Raw`, `V1`
and `V2`. `Protocol::Connection<Counterpart>` is the connection type builder
callbacks receive (`ConnectionTo` for `Raw`, `v1::ConnectionTo` for `V1`,
`v2::ConnectionTo` for `V2`), so code generic over a builder's context changes
`Context: ConnectionContext` to `P: Protocol`.

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
ACP message into its params and sends optional metadata as `_meta` (`meta` is
also accepted on receive).

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

### Conductor internals are no longer exported

The conductor crate exports its API explicitly instead of through a glob.
`ConductorRunner`, `ConductorMessage`, `ConductorSuccessor`, `ComponentIndex`
and `SourceComponentIndex` are no longer public; they only described the
conductor's internal routing.

### Instantiators return `InstantiatedChain` and `InstantiatedProxies`

`InstantiateProxiesAndAgent` and `InstantiateProxies`, including instantiator
closures, return `#[non_exhaustive]` structs instead of tuples. The
`ProxiesAndAgentComponents` type alias is removed.

| Before | After |
| --- | --- |
| `Ok((request, proxies, agent))` | `Ok(InstantiatedChain::new(request, proxies, agent))` |
| `Ok((request, proxies))` | `Ok(InstantiatedProxies::new(request, proxies))` |
| `ProxiesAndAgentComponents` | `InstantiatedChain` |

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

The `unstable_protocol_v2` and `unstable_mcp_over_acp` passthrough features are
unchanged and apply to every rmcp module.

Then add `rmcp_3::` to every import from the crate:

| Before | After |
| --- | --- |
| `agent_client_protocol_rmcp::McpServerExt` | `agent_client_protocol_rmcp::rmcp_3::McpServerExt` |
| `agent_client_protocol_rmcp::McpServerBuilder` | `agent_client_protocol_rmcp::rmcp_3::McpServerBuilder` |
| `agent_client_protocol_rmcp::McpTool` | `agent_client_protocol_rmcp::rmcp_3::McpTool` |
| `agent_client_protocol_rmcp::EnabledTools` | `agent_client_protocol_rmcp::rmcp_3::EnabledTools` |
| `agent_client_protocol_rmcp::tool_fn!` | `agent_client_protocol_rmcp::rmcp_3::tool_fn!` |
| `agent_client_protocol_rmcp::tool_fn_mut!` | `agent_client_protocol_rmcp::rmcp_3::tool_fn_mut!` |

`McpTool`, `EnabledTools`, `tool_fn!` and `tool_fn_mut!` are re-exports of the
core crate's items, which you can also import from
`agent_client_protocol::mcp_server` and `agent_client_protocol` directly.

Future rmcp major versions arrive as new features and modules (such as
`rmcp_4`) in minor releases, so you can enable both while moving your services
to the new rmcp major version. Removing support for an rmcp major version is a
breaking change and only happens in an ACP major release.
