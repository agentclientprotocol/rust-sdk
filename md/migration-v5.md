# Migrating to v5

This page covers the breaking changes in `agent-client-protocol` 5.0 and the
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
