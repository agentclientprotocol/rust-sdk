# HTTP / WebSocket Transport

`agent-client-protocol-http` exposes ACP agents over one `/acp` endpoint.

- `POST /acp` with `initialize` creates a connection and returns `Acp-Connection-Id`.
- Later `POST /acp` requests include `Acp-Connection-Id`; session-scoped requests also include `Acp-Session-Id` or `params.sessionId`.
- `GET /acp` with `Accept: text/event-stream` streams agent messages over SSE. Use a connection-level stream for connection-scoped messages and per-session streams for session-scoped messages.
- `GET /acp` with a WebSocket upgrade uses text frames for JSON-RPC messages.
- `DELETE /acp` tears down the connection.

`POST /acp` request bodies are limited to 16 MiB.

## JSON-RPC Batches

`HttpClient` starts every connection with an individual `initialize` and
requires an individual initialize response. For compatibility with other
clients, the server also accepts an initial batch when its first call-shaped
entry is an `initialize` request. Valid and malformed response-only entries may
precede it and are ignored; an invalid or call-shaped predecessor rejects the
batch as an initial frame. The server forwards the complete frame and returns
the complete grouped response in the POST response body; a successful
initialize also adds `Acp-Connection-Id`. Lifecycle-sensitive calls should
normally remain individual. If the agent emits a notification or callback
before the initialize response is ready, including from a batched sibling, the
server buffers that frame for the connection's SSE stream until initialization
completes.

After initialization, both transport shapes preserve batches:

- On an established HTTP connection, one complete batch occupies one POST
  body. The server returns `202 Accepted`; any grouped JSON-RPC reply is
  delivered through SSE as one array.
- WebSocket sends one complete batch in one text frame and writes its grouped
  reply in one text frame.
- A grouped HTTP reply is sent to a session stream only when all correlated
  entries have the same session route. If routes differ, it is sent on the
  connection-level stream so the array remains intact.

Entry validation, notification-only behavior, empty arrays, and malformed
response filtering follow the shared [transport batch
contract](./transport-architecture.md#json-rpc-batch-behavior).

## HTTP + SSE Streams

After `initialize`, clients should open a connection-level SSE stream:

- `GET /acp`
- `Accept: text/event-stream`
- `Acp-Connection-Id: <connection id>`
- no `Acp-Session-Id`

This stream carries connection-scoped messages.

Session-scoped messages are routed to session-specific SSE streams. For each
active session, clients should also open:

- `GET /acp`
- `Accept: text/event-stream`
- `Acp-Connection-Id: <connection id>`
- `Acp-Session-Id: <session id>`

For a session not yet used on this HTTP connection, send its first
session-scoped POST (for example, `session/load` or `session/resume`) and wait
for `202 Accepted` before opening its GET. The POST registers the bounded
session mailbox before it is admitted to the agent. History and other output
can then queue until the GET attaches; an unknown-session GET returns 409 and
does not allocate a mailbox. A batch registers all its session mailboxes before
returning 202.

The server reserves inbound queue capacity before publishing that metadata.
A rejected POST therefore cannot roll back a mailbox another accepted POST
has adopted. Registration failure or cancellation before publication releases
the reserved queue slot without leaving partial metadata.

Keep the connection stream and existing session streams running during this
setup so callback responses and cancellation can still progress. `HttpClient`
does this automatically. When a `session/new` or `session/fork` response returns
a new `sessionId`, its mailbox is already registered and the client can open
the corresponding GET immediately.

Frame and pending-work limits apply to HTTP and WebSocket traffic. Session
metadata retains a measured charge for its ID, not the entire opening request.
Consuming an SSE/WebSocket frame releases its payload charge before waiting
for the next frame. A WebSocket frame rejected by admission terminates that
connection rather than silently losing input or waiting forever to drain a
still-live agent.

HTTP clients cap decoded SSE data at `max_frame_bytes` and each complete raw
event at that limit plus a 16 KiB envelope allowance. CR, LF, and CRLF line
endings are supported even when split across transport chunks.

A fatal outbound routing failure, such as mailbox overflow, closes the whole
connection and reclaims its agent and metadata even if the agent emits no more
messages. This is distinct from an ordinary SSE disconnect, which can reconnect
to the existing mailbox.

## Features

The crate does not enable either transport side by default. Opt into only the side(s) you need.

```toml
agent-client-protocol-http = { version = "...", features = ["client"] }
agent-client-protocol-http = { version = "...", features = ["server"] }
agent-client-protocol-http = { version = "...", features = ["client", "server"] }
```

The `client` feature exposes `HttpClient`. The `server` feature exposes
`AcpHttpServer`, `ServerOptions`, and `CorsOptions`.

## Request Cancellation

Request cancellation is available through the core SDK:

```toml
agent-client-protocol-http = { version = "...", features = ["client", "server"] }
```

`$/cancel_request` is connection-scoped. The HTTP transport does not apply
`Acp-Session-Id` to cancellation notifications, and routes outgoing
cancellation notifications over the connection stream rather than a session
stream. Cancellation is advisory: a `202 Accepted` for the cancellation POST
does not complete the original request. Pending response routing and its
bounded metadata charge stay in place until a terminal response, POST failure,
or connection teardown. The original request can still succeed after
cancellation, including a `session/new` or `session/fork` that opens a new
session stream. If the peer never sends a response, that request continues to
occupy pending-request capacity until the connection closes.

WebSocket connections can carry cancellation at any point after the socket is
open. With HTTP + SSE, cancellation can be sent after `initialize` completes and
the client has received `Acp-Connection-Id`; an in-flight `initialize` request
cannot be cancelled with a hop-local `$/cancel_request` on this transport shape.

## Server

```rust
use agent_client_protocol_http::AcpHttpServer;

let app = AcpHttpServer::new(|| my_agent()).into_router();
let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
axum::serve(listener, app).await?;
```

Cross-origin browser access is disabled by default. Enable it by allowlisting
the browser origins that should be able to access the ACP endpoint:

```rust
use agent_client_protocol_http::{AcpHttpServer, CorsOptions, ServerOptions};

let app = AcpHttpServer::new(|| my_agent())
    .with_options(ServerOptions {
        cors: CorsOptions::allow_origins(["http://localhost:5173"])?,
        ..ServerOptions::default()
    })
    .into_router();
```

## Client

```rust
use agent_client_protocol_http::HttpClient;

let transport = HttpClient::new("http://127.0.0.1:8080")?;
my_client().connect_to(transport).await?;
```

The same `HttpClient` also speaks WebSocket — pass a `ws://` or `wss://` URL
and it will open a single bidirectional connection instead of using POST + SSE:

```rust
let transport = HttpClient::new("ws://127.0.0.1:8080")?;
my_client().connect_to(transport).await?;
```
