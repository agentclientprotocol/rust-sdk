# Migrating to v4

This page covers the breaking changes in `agent-client-protocol` 4.0 and the
crates released with it.

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
| `Raw` (the default) | `HandleDispatchFrom`, `MatchDispatch`, `ConnectionTo::send_request`, builders without a version guard, `Builder::new` | `&RawJsonRpcId` |
| `V1` | typed handlers on `Client.builder()`, `Agent.builder()`, `Proxy.builder()` | `&schema::v1::RequestId` |
| `V2` (`unstable_protocol_v2`) | typed handlers on `Client.v2()`, `Agent.v2()`, `Proxy.v2()`; `V2ConnectionTo` | `&schema::v2::RequestId` |

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

Typed handlers on `Client.builder()`, `Agent.builder()` and `Proxy.builder()`
receive `V1` handles, so `id()` still returns `&schema::v1::RequestId`.
Closures whose parameters are not annotated need no change. Where the handle
type is spelled out, add the marker, because the default is now `Raw`:

```rust,ignore
use agent_client_protocol::{Dispatch, Responder, V1};

// Before
Agent.builder().on_receive_request(
    async |request: PromptRequest, responder: Responder<PromptResponse>, cx| { ... },
    on_receive_request!(),
)
// After
Agent.builder().on_receive_request(
    async |request: PromptRequest, responder: Responder<PromptResponse, V1>, cx| { ... },
    on_receive_request!(),
)

// Likewise for on_receive_dispatch
async |message: Dispatch<PromptRequest, SessionNotification, V1>, cx| { ... }
```

`Client.builder()`, `Agent.builder()` and `Proxy.builder()` return
`Builder<_, _, _, _, V1>`; code that names that type adds the marker too.
`Builder::new`, other roles' builders, and
`without_acp_version_guard()` / `with_v2_protocol_guard()` (which now return
a `Raw` builder) use `Raw`. The v1 session helpers that take a responder,
such as `SessionBuilder::on_proxy_session_start`, take a `Responder<_, V1>`.

### v2 code

The `v2_id()` accessors are removed. `id()` on handles from the v2 builders and
`V2ConnectionTo` returns the v2 ID directly:

```rust,ignore
// Before
let id: Option<v2::RequestId> = responder.v2_id();
let id: &v2::RequestId = sent.v2_id();

// After
let id: &v2::RequestId = responder.id();
let id: &v2::RequestId = sent.id();
```

Closures registered on a v2 builder whose parameters are not annotated need
no change. Where a v2 handle type is spelled out, add the marker:

```rust,ignore
use agent_client_protocol::{Responder, SentRequest, V2};

// Before
async |request: v2::PromptRequest, responder: Responder<v2::PromptResponse>, cx| { ... }
fn send(session: &V2Session<Agent>) -> SentRequest<v2::PromptResponse> { ... }

// After
async |request: v2::PromptRequest, responder: Responder<v2::PromptResponse, V2>, cx| { ... }
fn send(session: &V2Session<Agent>) -> SentRequest<v2::PromptResponse, V2> { ... }
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
callbacks receive (`ConnectionTo` for `Raw` and `V1`, `V2ConnectionTo` for
`V2`), so code generic over a builder's context changes
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

Code that uses the SDK types (`InitializeProxyRequest`, `SuccessorMessage`,
`METHOD_INITIALIZE_PROXY`, `METHOD_SUCCESSOR_MESSAGE`, or the `Proxy` and
`Conductor` roles) needs no source changes. Update any hard-coded method
strings, such as in raw JSON-RPC handlers, test fixtures, or trace filters.

### `ConductorHostRole` is sealed

`agent-client-protocol-conductor::ConductorHostRole` can no longer be
implemented outside the conductor crate. Its methods take a `ConductorRunner`,
whose fields are private, so no external implementation could be written
before either. Use the provided `Agent` and `Proxy` host roles.
