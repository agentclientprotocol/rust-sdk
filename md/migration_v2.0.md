# Migrating from `agent-client-protocol` 1.x to 2.0

Version 2.0 makes JSON-RPC notification semantics explicit, changes the low-level in-process
transport boundary so frames remain intact across components and adapters, clarifies the
distinction between responding to requests and routing responses, makes dynamic handler lifetimes
explicit, and gives `AcpAgent` an SDK-owned process-launch configuration instead of reusing an MCP
wire-schema type.

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
explicit malformed wire value. Transport I/O failures are returned by the future from
`ConnectTo::into_channel_and_future`; they are never channel items. Components should preserve a
received frame intact when relaying it so batch response grouping remains correct.

The separate, hidden `FramedChannel` and `into_framed_channel_and_future` compatibility path no
longer exist. Implement only `ConnectTo::connect_to`, optionally overriding
`into_channel_and_future` for a direct channel adapter.

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

## Matchers use dispatch terminology

The combined matchers operate on `Dispatch` values, which can represent requests, notifications,
or responses. Their method names now reflect that input:

| 1.x | 2.0 |
| --- | --- |
| `MatchDispatch::if_message` | `MatchDispatch::if_dispatch` |
| `MatchDispatchFrom::if_message_from` | `MatchDispatchFrom::if_dispatch_from` |

## Connection and session accessors borrow

`McpConnectionTo::acp_id` now returns `&str`. The deprecated `acp_url` alias was removed; use
`acp_id` instead. `McpConnectionTo::connection_to` is now `connection` and returns
`&ConnectionTo<_>`.

`ActiveSession::modes` and `ActiveSession::meta` now return `Option<&T>` instead of `&Option<T>`,
and `ActiveSession::connection` returns `&ConnectionTo<_>`.

These accessors avoid implicit allocation and handle cloning. Call `.to_owned()` on `acp_id`,
`.cloned()` on `modes` or `meta`, and `.clone()` on either connection accessor when an owned value
is required.

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
`block_task()` followed by `run_until` or `start_session`. Proxy handlers can use
`on_proxy_session_start`, or `block_task().start_session_proxy(...)`. Directly attaching an
already-returned `NewSessionResponse` is no longer supported, so move request customization into
`build_session_from` before the builder sends `session/new`.

Construct `Lines` and `ByteStreams` with `Lines::new(outgoing, incoming)` and
`ByteStreams::new(outgoing, incoming)`; their stream fields are no longer public.

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
