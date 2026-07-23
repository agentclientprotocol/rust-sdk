# Migrating from `agent-client-protocol` 1.x to 2.0

Version 2.0 makes JSON-RPC notification semantics explicit, changes the low-level in-process
transport boundary so frames remain intact across components and adapters, clarifies the
distinction between responding to requests and routing responses, makes dynamic handler lifetimes
explicit, and gives `AcpAgent` an SDK-owned process-launch configuration instead of reusing an MCP
wire-schema type. It also replaces the SDK-local MCP-over-ACP wire extension with the shared
schema's opt-in native transport.

## Compatible crate versions

Most published workspace crates move to the 2.x version family together. The rmcp integration
moves to 3.x because both `agent-client-protocol` and `rmcp` are public dependencies in its API:

| Crate | Version compatible with `agent-client-protocol` 2.x |
| --- | --- |
| `agent-client-protocol` | 2.x |
| `agent-client-protocol-derive` | 2.x |
| `agent-client-protocol-conductor` | 2.x |
| `agent-client-protocol-cookbook` | 2.x |
| `agent-client-protocol-http` | 2.x |
| `agent-client-protocol-polyfill` | 2.x |
| `agent-client-protocol-trace-viewer` | 2.x |
| `agent-client-protocol-rmcp` | 3.x |

## Notifications cannot receive error responses

The SDK no longer exposes `ConnectionTo::send_error_notification`, and
`Dispatch::respond_with_error` has been removed. Match the dispatch variant when a catch-all
handler needs different behavior:

```rust
use agent_client_protocol::{Dispatch, Error};

# fn handle(message: Dispatch) -> Result<(), Error> {
match message {
    Dispatch::Request(_, responder) => {
        responder.respond_with_error(Error::method_not_found())
    }
    Dispatch::Notification(_) => Ok(()),
    Dispatch::Response(result, router) => router.route_with_result(result),
}
# }
```

In most applications, omit the catch-all handler entirely. The built-in fallback responds to
unknown requests with `Method not found`, ignores unhandled notifications, and routes responses
to their pending requests.

`TypeNotification` no longer has a role parameter or takes a connection:

```rust
use agent_client_protocol::util::TypeNotification;

// 1.x
// TypeNotification::<Peer>::new(message, &connection)

// 2.0
TypeNotification::new(message)
# ;
```

The notification type parameter of `Dispatch<Req, Notif>` now requires
`Notif: JsonRpcNotification`, matching the variant it can contain. The duplicate
`Dispatch::erase_to_json` method was removed; use `into_untyped_dispatch`.

## Raw channels carry frames

`Channel` is now the single batch-aware in-process transport boundary. Its `rx` and `tx` carry
`TransportFrame`, not `Result<RawJsonRpcMessage, Error>`:

```rust
use agent_client_protocol::{Channel, RawJsonRpcMessage, TransportFrame};

# fn send(channel: &Channel, message: RawJsonRpcMessage) {
channel.tx.unbounded_send(TransportFrame::Single(message)).unwrap();
# }
```

`TransportFrame` distinguishes a valid single message, a non-empty `TransportBatch`, and an
explicit malformed wire value. Each batch contains public `TransportBatchEntry` values so a
relay can retain invalid siblings without turning a protocol error into a transport failure.
All three public frame types implement `Clone`, and `TransportBatch::into_entries()` provides an
owned, source-order iterator.

Transport I/O failures are returned by the future from
`ConnectTo::into_channel_and_future`; they are never channel items. Components should preserve a
received frame intact when relaying it so batch response grouping remains correct.

`TransportFrame::parse_json` returns a frame directly for every input. It retains malformed
response-shaped values so raw framed intermediaries can preserve them; protocol actors suppress
replies to response-only shapes. Standalone malformed input keeps its exact source text. Batch
entries retain parsed JSON values and source order, but reserialization may normalize whitespace.

The standard line, byte-stream, stdio, HTTP, and WebSocket transports now accept incoming
JSON-RPC batches in both protocol v1 and v2 mode. Entries are handled independently, replies are
grouped into one response array, notification-only batches produce no reply, and an empty array
produces one standalone `Invalid Request` response. Malformed entries that are response-shaped but
not call-shaped are ignored because JSON-RPC responses must not themselves receive responses. The
SDK does not initiate batches of requests or notifications. See
[Transport Architecture](./transport-architecture.md#json-rpc-batch-behavior) for the full framing
contract.

Dropping a `Responder` for one request in a batch now produces an `Internal Error` fallback after
dispatch completes, allowing completed sibling responses to flush. A handler error overrides the
fallback with that error. The longstanding behavior for an individual request is unchanged:
dropping its responder does not automatically send a response.

Existing `ConnectTo` implementations that override `into_channel_and_future` must now return the
frame-aware `Channel`. Most components need to implement only `ConnectTo::connect_to`; a direct
channel adapter may still override `into_channel_and_future` to avoid an intermediate copy.

## Response routing uses routing terminology

`ResponseRouter` completes a local pending request; it does not send a new JSON-RPC response.
Its methods have therefore been renamed:

| 1.x | 2.0 |
| --- | --- |
| `respond_with_result` | `route_with_result` |
| `respond` | `route` |
| `respond_with_error` | `route_with_error` |
| `respond_with_internal_error` | `route_with_internal_error` |

`Responder` still uses `respond*`, because it sends the response to an incoming request.

If a catch-all response handler returns `Err`, that error is now routed to the local
`SentRequest` awaiter. It is never serialized as a response to the peer's response. This replaces
the misleading generic failure that previously appeared when the real interceptor error was lost.

## Response callbacks enforce ordered dispatch

The 1.x documentation said that `on_receiving_result` and `on_receiving_ok_result` callbacks held
the dispatch loop until completion, but the implementation did not enforce that ordering. In 2.0,
registering either callback before a peer response is routed during its original dispatch selects
ordered consumption: registration returns immediately, then the loop waits for response handling
to finish before processing the next message.

This is a behavioral change. An ordered response callback must not await a later response,
notification, or other inbound traffic on the same connection, because the loop cannot dispatch
that traffic until the callback returns. Spawn that follow-up work and return from the callback,
or use `block_task` from a task that already runs outside the dispatch loop, such as the foreground
future passed to `connect_with`.

The barrier is not retroactive. A pending-request failure delivered without an incoming response
(such as EOF), a response that was already routed, or a retained `ResponseRouter` routed after its
original dispatch runs without holding the loop.

Session-start helpers apply this barrier only to framework-owned runner and routing installation,
then spawn the user callback. This preserves the actual 1.x scheduling behavior for user session
work while correcting the old documentation that claimed the user callback itself blocked
dispatch.

## Request IDs remain typed

`Responder::id`, `ResponseRouter::id`, and `SentRequest::id` now return `&RequestId`.
`Dispatch::id` returns `Option<&RequestId>`. Clone the ID when it must outlive the handle:

```rust
let id = sent_request.id().clone();
```

This removes JSON round-trips and lets IDs pass directly to APIs such as request cancellation.
If an integration still needs an untyped JSON value, serialize the borrowed ID explicitly:

```rust
let id_json = serde_json::to_value(sent_request.id())?;
let dispatch_id_json = dispatch.id().map(serde_json::to_value).transpose()?;
# let _ = (id_json, dispatch_id_json);
```

## Dynamic handlers use a guard

`DynamicHandlerRegistration` is now `DynamicHandlerGuard` and is exported from the crate root.
The guard is `must_use` and no longer `Clone`: keep its single owner in the object that owns the
registration. Dropping it unregisters the handler. To leave a handler registered for the rest of
the connection, replace `run_indefinitely()` with `detach()`. Detaching no longer leaks an extra
`ConnectionTo` handle.

## Background tasks use runner terminology

Builder extensions implementing `RunWithConnectionTo` run alongside the connection; they do not
respond to an individual JSON-RPC request. The builder method now reflects that distinction:

| 1.x | 2.0 |
| --- | --- |
| `Builder::with_responder` | `Builder::with_runner` |

The conductor crate applies the same terminology to its public background task:

| 1.x | 2.0 |
| --- | --- |
| `agent_client_protocol_conductor::ConductorResponder` | `agent_client_protocol_conductor::ConductorRunner` |

This is a type rename only; custom code that names the conductor task should update its imports
and type references.

## Matchers use dispatch terminology

The combined matchers operate on `Dispatch` values, which can represent requests, notifications,
or responses. Their method names now reflect that input:

| 1.x | 2.0 |
| --- | --- |
| `MatchDispatch::if_message` | `MatchDispatch::if_dispatch` |
| `MatchDispatchFrom::if_message_from` | `MatchDispatchFrom::if_dispatch_from` |

## Connection and session accessors borrow

`McpConnectionTo::acp_id` is now `server_id` and returns `Option<&McpServerAcpId>`. The new name
matches the native `McpServer::Acp` declaration; the `Option` reflects that a server can also be
connected directly without ACP. `connection_id` returns an `Option<&McpConnectionId>` for the
distinct active connection created by `mcp/connect`. Use `context()` to match explicitly on
`McpConnectionContext::Standalone` or `McpConnectionContext::Acp { server_id, connection_id }`.
The deprecated `acp_url` alias was removed. `McpConnectionTo::connection_to` is now `connection`
and returns `&ConnectionTo<_>`.

`ActiveSession::modes` and `ActiveSession::meta` now return `Option<&T>` instead of `&Option<T>`,
and `ActiveSession::connection` returns `&ConnectionTo<_>`.

These accessors avoid implicit allocation and handle cloning. Call `.cloned()` on `server_id()`,
`connection_id()`, `modes`, or `meta`, and `.clone()` on either connection accessor when an owned
value is required.

## MCP servers use the native opt-in transport

The runtime-agnostic `agent_client_protocol::mcp_server` module remains available without an
unstable ACP feature, so standalone MCP servers do not allocate or retain schema transport IDs.
Enable the feature when attaching a server to ACP with `Builder::with_mcp_server` or
`SessionBuilder::with_mcp_server`:

```toml
agent-client-protocol = { version = "2", features = ["unstable_mcp_over_acp"] }
```

`agent-client-protocol-rmcp` no longer enables this feature merely to build or directly serve an
MCP server. Applications that attach an rmcp-backed server to ACP should enable its matching
`unstable_mcp_over_acp` passthrough feature. The transport remains unstable and may change
independently of the stable ACP surface.

In 1.x, the SDK represented an ACP-provided MCP server as `McpServer::Http` with an `acp:` URL and
routed it through SDK-local underscore-prefixed methods. In 2.0, providers and native consumers
use:

- `McpServer::Acp(McpServerAcp { name, server_id, .. })` in session setup requests;
- `mcp/connect` with `serverId`, returning a distinct `connectionId`;
- `mcp/message` requests and notifications keyed by that connection ID; and
- an `mcp/disconnect` request with an empty response.

The low-level SDK-local `McpConnectRequest`, `McpConnectResponse`, `McpOverAcpMessage`, and
`McpDisconnectNotification` types were removed. Use the feature-gated schema types instead:

| 1.x SDK-local type | 2.0 schema type |
| --- | --- |
| `McpConnectRequest` | `schema::v1::ConnectMcpRequest` |
| `McpConnectResponse` | `schema::v1::ConnectMcpResponse` |
| `McpOverAcpMessage` request | `schema::v1::MessageMcpRequest` |
| `McpOverAcpMessage` notification | `schema::v1::MessageMcpNotification` |
| `McpDisconnectNotification` | `schema::v1::DisconnectMcpRequest` and `DisconnectMcpResponse` |

The public method-name constants moved to the schema's generated method-name
tables:

| 1.x SDK-local constant | 2.0 schema constant |
| --- | --- |
| `METHOD_MCP_CONNECT_REQUEST` | `schema::v1::CLIENT_METHOD_NAMES.mcp_connect` |
| `METHOD_MCP_MESSAGE` | `schema::v1::CLIENT_METHOD_NAMES.mcp_message` or `AGENT_METHOD_NAMES.mcp_message`, depending on direction |
| `METHOD_MCP_DISCONNECT_NOTIFICATION` | `schema::v1::CLIENT_METHOD_NAMES.mcp_disconnect` |

Code using `Builder::with_mcp_server` or `SessionBuilder::with_mcp_server` continues to attach the
high-level server in the same place; the emitted declaration and wire methods change. Global
builder attachment advertises the same server ID on `session/new`, `session/load`,
`session/resume`, and feature-gated `session/fork`. Per-session attachment remains specific to
`session/new`. Do not construct an HTTP server with an `acp:` URL. If the final agent accepts HTTP
but not native ACP MCP servers, insert `McpOverAcpPolyfill` immediately before it. The polyfill now
consumes native `McpServer::Acp` declarations and adapts only its final-agent-facing side.

The polyfill's public `BridgeMode` enum and `McpOverAcpPolyfill::stdio` were removed because the
required conductor `mcp` helper subcommand no longer exists. The polyfill has one supported mode;
construct it with `McpOverAcpPolyfill::http()` or `Default`, or manage a standard MCP transport
separately.

## Low-level helpers have a narrower surface

`DynConnectTo::type_name` now returns `&'static str` without allocating. Call `.to_owned()` when
an owned type name is required.

The generic `util::both` helper was removed. Replace `util::both(a, b).await` with
`futures::future::try_join(a, b).await.map(|((), ())| ())`.

`util::process_stream_concurrently` is no longer public. An equivalent unbounded fallible loop can
be written with `futures::{StreamExt, TryStreamExt}`:

```rust,ignore
stream
    .map(Ok::<_, agent_client_protocol::Error>)
    .try_for_each_concurrent(None, |item| process_fn(item))
    .await
```

Pass a finite limit instead of `None` to bound concurrency.

`ConnectionTo::attach_session` is no longer public. Create sessions through
`ConnectionTo::build_session`, `build_session_cwd`, or `build_session_from` instead. Use
`SessionBuilder::on_session_start` to start without blocking the calling task, or call
`block_task()` followed by `run_until` or `start_session` outside message handlers. Proxy handlers
must use `on_proxy_session_start`; only call `block_task().start_session_proxy(...)` from a task
already outside the dispatch loop, such as a `connect_with` foreground future or a spawned task.
Directly attaching an already-returned `NewSessionResponse` is no longer supported, so move
request customization into `build_session_from` before the builder sends `session/new`.

When the session response is routed during its original dispatch, `on_session_start` and
`on_proxy_session_start` install session routing under the ordered response callback, then spawn
the user callback. No user callback code runs under that ordering guarantee. The callback itself
must be `'static`, but its returned future does not need an additional `'static` bound and may
safely wait for later connection or session traffic. A response interceptor that retains and
routes the response later cannot retroactively order that setup before already-processed messages.
Register application state needed for routing before calling these helpers. Bookkeeping that
requires the returned session or session ID runs concurrently with later traffic. If handlers
must observe ID-keyed bookkeeping first, install a gate or placeholder before calling the helper,
have those handlers await it, and populate it from the callback.

Construct `Lines` and `ByteStreams` with `Lines::new(outgoing, incoming)` and
`ByteStreams::new(outgoing, incoming)`; their stream fields are no longer public.

## Draft v2 schema updates

The optional `unstable_protocol_v2` surface now tracks
`agent-client-protocol-schema` 1.5. Because this API is explicitly unstable, its source changes
are included in the SDK 2.0 migration rather than treated as stable-v1 wire changes.

- Many values that were plain `String` or `PathBuf` fields are semantic newtypes, including
  `AbsolutePath`, `MediaType`, session/message/tool/terminal IDs, and list cursors. Construct them
  with `.into()` or their `new` methods, and use `AsRef<Path>` or `as_ref()` when borrowing their
  contents.
- `v2::DiffPatch.diff` is now `text`. `DiffPatch::new(text)` remains the preferred constructor.
- Terminal state is represented by `Terminal`, `TerminalUpdate`, `TerminalOutput`,
  `TerminalOutputChunk`, and `TerminalExitStatus`. `SessionUpdate` also has terminal update and
  output-chunk variants, so exhaustive matches must handle the new variants.
- Conversion helpers are now generic and fallible. Replace `v2_to_v1(value)` and
  `v1_to_v2(value)` with `try_v2_to_v1::<_, Target>(value)` and
  `try_v1_to_v2::<_, Target>(value)`. Use `try_v2_to_v1_many::<_, Target>(value)` when one v2
  update may become several v1 updates.
- The bespoke `IntoV1`, `IntoV1Many`, and `IntoV2` conversion traits have been removed. Use the
  standard `TryFrom`/`TryInto` traits for fallible conversions, `From`/`Into` for infallible
  conversions, or the helper functions above.
- `v2::SessionCapabilities::into_v1()` is now `try_into_v1_parts()`.

## `SentRequest::map` accepts arbitrary output

`SentRequest::map` can now consume a typed response into any output type; the mapped value no
longer needs to implement `JsonRpcResponse`. This includes mapped values carrying non-`'static`
lifetimes when they are consumed with `block_task`; callback-style consumption still requires
`'static` because it is spawned onto the connection. The mapper may also be a one-shot closure.
This is additive, so existing mapping code does not need to change.

## `AcpAgent` has its own process configuration

`AcpAgent` now accepts `AcpAgentConfig` instead of the ACP wire-schema `McpServer`. An ACP agent
subprocess is not an MCP server, and its local launch settings should not depend on the v1
protocol schema:

```rust
use agent_client_protocol::{AcpAgent, AcpAgentConfig};

let agent = AcpAgent::new(
    AcpAgentConfig::new("my-agent")
        .arg("--verbose")
        .env("RUST_LOG", "info"),
);
let configuration = agent.config();
# let _ = configuration;
```

`server()` and `into_server()` are now `config()` and `into_config()`. The MCP-only `name` and
`_meta` fields have no equivalents because they never affected process launching. Use `command()`,
`arguments()`, and `environment()` to inspect the configuration.

The JSON configuration shape also changes. The MCP `type`, `name`, and `_meta` fields are removed,
and environment variables change from schema objects:

```json
{
  "type": "stdio",
  "name": "my-agent",
  "command": "python",
  "args": ["agent.py"],
  "env": [{ "name": "RUST_LOG", "value": "info" }]
}
```

to a string map:

```json
{
  "command": "python",
  "args": ["agent.py"],
  "env": { "RUST_LOG": "info" }
}
```

Command-string and `from_args` construction are unchanged. HTTP and SSE `McpServer` variants were
never valid subprocess launch configurations; callers using them must select an appropriate
network transport separately.

The deprecated `AcpAgent::zed_claude_code` and `AcpAgent::zed_codex` constructors were removed;
use `claude_agent` and `codex`. The `google_gemini` convenience constructor was also removed;
replace it with the explicit command:

```rust
let agent = AcpAgent::from_args([
    "npx",
    "-y",
    "--",
    "@google/gemini-cli@latest",
    "--experimental-acp",
]).unwrap();
```
