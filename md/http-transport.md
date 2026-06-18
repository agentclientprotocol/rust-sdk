# HTTP / WebSocket Transport

`agent-client-protocol-http` exposes ACP agents over one `/acp` endpoint.

- `POST /acp` with `initialize` creates a connection and returns `Acp-Connection-Id`.
- Later `POST /acp` requests include `Acp-Connection-Id`; session-scoped requests also include `Acp-Session-Id` or `params.sessionId`.
- `GET /acp` with `Accept: text/event-stream` streams agent messages over SSE. Use a connection-level stream for connection-scoped messages and per-session streams for session-scoped messages.
- `GET /acp` with a WebSocket upgrade uses text frames for JSON-RPC messages.
- `DELETE /acp` tears down the connection.

`POST /acp` request bodies are limited to 16 MiB.

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

Request cancellation is available when the transport crate forwards the core SDK
feature:

```toml
agent-client-protocol-http = { version = "...", features = ["client", "server", "unstable_cancel_request"] }
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
