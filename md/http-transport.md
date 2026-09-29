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

Open a session stream before sending methods such as `session/prompt`,
`session/load`, `session/resume`, or other session-scoped requests. When a
`session/new` or `session/fork` response returns a new `sessionId`, open an SSE
stream for that returned session before expecting updates or responses for it.

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
stream.

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

## Opt-in Bounded HTTP/SSE

Existing constructors preserve the compatibility-only unbounded transport.
Select finite admission explicitly on each endpoint:

```rust
use agent_client_protocol_http::{AcpHttpServer, HttpClient, HttpClientLimits, ServerLimits};

let app = AcpHttpServer::new_bounded(|| my_agent(), ServerLimits::default())?
    .into_router();
let transport = HttpClient::new("http://127.0.0.1:8080/acp")?
    .with_limits(HttpClientLimits::default())?;
my_client().connect_to(transport).await?;
```

These return `BoundedAcpHttpServer` and `BoundedHttpClient`. The server retains
`with_options(ServerOptions)` and `into_router()`. No fields were added to
`ServerOptions`. Both endpoints use the core bounded connection interface
directly, without forwarding through the legacy unbounded `Channel`.
Custom components must implement bounded extraction; unsupported legacy-only
components fail closed instead of silently losing the admission guarantee.
Raw adapters can use `BoundedHttpClient::into_bounded_channel_and_future()`;
they must poll the returned driver and retain each `ChargedFrame` until their
own consumption boundary. See [bounded core transports](./transport-architecture.md)
for producer admission and charged frame ownership.

### Finite Defaults

Zero limits are invalid. The limits structures are public and can be configured
before construction. Budgets are independent and conservative: fitting one
limit does not guarantee admission through every other limit.

| Budget | Server default | Client default |
| --- | --- | --- |
| Core maximum serialized frame | 256 KiB | 1 MiB |
| Core reserved bytes / frames, each direction | 16 MiB / 256 | 16 MiB / 256 |
| Core pending requests / tasks | 256 / 256 | 256 / 256 |
| Active logical connections | 64 | One per transport |
| Concurrent POSTs / aggregate request-body reservation | 32 / 8 MiB | Separate request and response lanes |
| HTTP maximum request frame / batch entries | 256 KiB / 128 | Core frame limit |
| HTTP egress bytes / frames | 4 MiB / 64 per connection | 64 MiB / 128 HTTP reservations |
| Pending routed RPC entries | 256 per connection | 128 |
| Registered sessions / active SSE streams | 64 / 65 per connection | 8 streams, including connection stream |
| Queued plus active POSTs | Global concurrent POST admission | 32 request and 32 response-only |
| Response body / SSE line / event / chunk | Outbound frame and egress limits | 1 MiB each |

Client reservations cover POST bodies, pending metadata, bounded response
workspaces, and SSE parser workspaces; each reservation also consumes a frame
slot. Server POST bodies reserve the maximum frame size before polling the body.
Server connection admission precedes factory invocation. Duplicate request IDs
consume separate pending entries; a successful POST does not imply RPC completion.
Response-only callback POSTs have an independent bounded client lane. Mixed
batches remain in request order. Registered server session mailboxes last until
connection termination and count against the configured session limit.

### Ownership and Exhaustion

Core producer admission happens before queueing. Charges survive intermediate
dequeue, routing, and body construction; the server releases yielded body
charges on the subsequent poll or body drop. Client bodies retain charges
through HTTP handoff. This bounds SDK-owned queued data and work, not an
application's cumulative output. A healthy consumer can process more than any
single budget over the connection's lifetime.

Exhaustion is explicit and fail-fast, not an unbounded queue of waiting sends.
The server rejects pre-acceptance overload with an HTTP error; terminal errors
revoke producers and release pending routes, mailboxes, and local tasks.
POST/body admission exhaustion for an addressed connection terminates that
connection so callbacks cannot remain indefinitely blocked behind saturated
request bodies. Cancellation/drop releases local reservations; client teardown
does not guarantee a remote DELETE completed.

Encoded-byte budgets are **not hard peak-heap limits**. Parsed JSON and bounded
serialization scratch add overhead; application conversion hooks can allocate
intermediate values before capped normalization. Allocator capacity, arbitrary
application task captures, and HTTP/TLS/socket buffers are outside encoded-byte
accounting. Core reservations and HTTP reservations are additional budgets, not
one shared process-memory counter.

### Delivery and Recovery Boundaries

A body poll transfers bytes to the HTTP stack. It is **not** proof of socket
flush, peer parsing, or ACP application consumption. An event already yielded
when SSE disconnects can be lost. No cursor/replay or exactly-once delivery is
provided, and no `Last-Event-ID` recovery is performed.

The bounded client does not automatically retry accepted or uncertain POSTs;
streaming request bodies are non-replayable, including with custom reqwest
redirect/retry policies. A lost response can leave acceptance unknown. Existing
JSON-RPC IDs remain correlation IDs, not idempotency keys.

The bounded path supports HTTP/SSE only. The bounded client rejects `ws`/`wss`
URLs, and bounded server WebSocket upgrades return HTTP 501. Legacy WebSocket
support is unchanged. ACP JSON-RPC messages and HTTP connection/session headers
are unchanged; bounded endpoints do not require a private protocol extension.
