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

The client validates the WebSocket handshake before sending any queued ACP
messages. This transport does not negotiate WebSocket subprotocols or extensions,
including compression. Setting `Sec-WebSocket-Protocol` or
`Sec-WebSocket-Extensions` in custom default headers does not enable support for
them; a server response selecting either is rejected.

### Client Configuration

Use the client builder to configure headers, proxies, DNS, TLS, and timeouts.
The configuration is used for both HTTP/SSE requests and WebSocket handshakes:

```rust
use std::time::Duration;
use agent_client_protocol_http::HttpClient;

let transport = HttpClient::builder("wss://agent.example")
    .configure_http(|http| {
        http.user_agent("my-acp-client")
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
    })
    .build()?;
```

`configure_http` transforms the current `reqwest::ClientBuilder`; repeated calls
retain earlier configuration. Use it for options such as `default_headers`,
`proxy`, `resolve`, `tls_certs_merge`, and `identity`. It does not open a
connection. URL validation and HTTP-client construction errors are returned by
`build()`, before connecting the transport.

`builder(base_url)` uses the same path rule as `new`: append `/acp` unless the
path already ends with it, after removing trailing slashes. For an exact
endpoint, including a custom server path or query string, use
`builder_with_endpoint(endpoint)` instead. `with_endpoint(endpoint)` remains
the unconfigured convenience constructor.

For WebSocket URLs, the SDK applies HTTP/1.1 and disables redirects **after**
the configuration callback. These are transport requirements, not defaults a
callback can override. A redirect fails at the original endpoint without sending
handshake headers to its destination; resolve the intended WebSocket endpoint
before connecting. HTTP/SSE retains the caller's HTTP-version and redirect
settings.

#### Proxy Headers

For WebSocket clients, **do not set `Host`, `Connection`, `Upgrade`, or any
`Sec-WebSocket-*` header through `reqwest::Proxy::headers`**. Use that API only
for non-handshake headers required by the proxy; proxy authentication can use
`Proxy::basic_auth` or `custom_http_auth`.

The SDK explicitly sets `Connection`, `Upgrade`, `Sec-WebSocket-Version`, and
`Sec-WebSocket-Key` over `default_headers`, but reqwest applies non-tunnel proxy
headers afterward on plain `ws://` requests and can overwrite those values.
The callback's proxy configuration is opaque: the SDK cannot
inspect, sanitize, or reject those settings at `build()`. This is a caller
configuration constraint, not an enforced builder restriction. Upgrade response
validation still precedes ACP data; it does not prove that every request header
was unchanged. For example, replacing the random key causes a response computed
from the replacement key to fail accept validation, but other reserved-header
overrides are not necessarily detected. For `wss://`, proxy headers apply to the
CONNECT request rather than the tunneled WebSocket handshake.

Reqwest's connection timeout covers connection establishment. Its request/read
timeouts also cover the opening WebSocket handshake but do not impose a lifetime
or idle timeout on the upgraded socket. For HTTP/SSE, request/read timeouts retain
their normal reqwest semantics, including the long-lived SSE response body.

For custom trust roots and client certificates, prefer reqwest's TLS options.
If using `tls_backend_preconfigured`, configure that backend's ALPN for
HTTP/1.1 yourself: reqwest does not rewrite a preconfigured TLS backend's ALPN,
even when HTTP/1.1 is selected on its builder. For rustls, set
`ClientConfig::alpn_protocols` to `vec![b"http/1.1".to_vec()]`. Incompatible
negotiation is rejected before any ACP messages are sent; HTTP/2 and HTTP/3
WebSocket negotiation is not implemented.

### Reusing an HTTP Client

To share an already-built reqwest client and its connection pool, use the
HTTP/SSE-only constructor with an **exact endpoint**, including `/acp` or the
server's custom path:

```rust
let http = reqwest::Client::builder().build()?;
let transport = HttpClient::from_http_client(
    "https://agent.example/acp",
    http.clone(),
)?;
```

`from_http_client` rejects `ws://` and `wss://` URLs before any network request.
An already-built reqwest client cannot be reconfigured to enforce the
WebSocket connection policies. Use the SDK builder for those URLs.

`HttpClient` itself can also be cloned to reuse its endpoint and underlying
HTTP client. Each connection has independent ACP transport state.

### Migrating Custom Client Construction

`HttpClient::with_client(base_url, client)` and
`with_endpoint_and_client(endpoint, client)` remain available but are deprecated.
Existing HTTP/SSE calls keep their path handling, supplied client, and connection
pool. They emit a deprecation warning so applications can migrate incrementally.

For `ws://` and `wss://`, both deprecated constructors now return
`HttpClientError::WebSocketRequiresBuilder` before any network I/O. Those callers
must migrate to the builder: retaining the old signatures cannot make an opaque,
already-built reqwest client enforce the WebSocket connection policies.

- For HTTP/SSE or WebSocket configuration, move the reqwest builder settings into
  `HttpClient::builder(base_url).configure_http(|http| ...).build()`. Do not call
  reqwest's `build()` inside the callback.
- For an exact endpoint, use `builder_with_endpoint(endpoint)` instead.
- To retain a shared, already-built HTTP/SSE client, use
  `from_http_client(exact_endpoint, client)`. Unlike the old base-URL constructor,
  this does not append `/acp`.
- Code using `new` or `with_endpoint` needs no source changes, but WebSocket
  proxy discovery and TLS trust defaults change as described below.

#### WebSocket Proxy and TLS Defaults

Unconfigured WebSockets intentionally share reqwest's defaults with HTTP/SSE.
Previously, the tungstenite connection was direct and WSS used bundled WebPKI
roots. Now `new`, `with_endpoint`, and unconfigured builders discover environment
proxies, honor system proxies when reqwest's `system-proxy` feature is enabled,
and use reqwest's TLS verifier. With this SDK's rustls feature selection, that
is platform certificate verification instead of the old bundled roots. Downstream
reqwest feature selection and explicit TLS configuration can affect this behavior.
Consequently, an unchanged WebSocket URL may take a different network route or
trust a different set of certificate authorities.

To disable proxy discovery and explicitly supplied proxies, opt into a direct
connection:

```rust
let transport = HttpClient::builder("wss://agent.example")
    .configure_http(|http| http.no_proxy())
    .build()?;
```

For deterministic trust, supply the application's root certificates with
reqwest 0.13's `tls_certs_only`, which disables platform/built-in roots:

```rust
use agent_client_protocol_http::HttpClient;

fn direct_with_root(root_pem: &[u8]) -> Result<HttpClient, Box<dyn std::error::Error>> {
    let root = reqwest::Certificate::from_pem(root_pem)?;
    Ok(HttpClient::builder("wss://agent.example")
        .configure_http(|http| http.no_proxy().tls_certs_only([root]))
        .build()?)
}
```

Supply the complete root set you intend to trust; this example does not recreate
the old bundled WebPKI root set. Use `tls_certs_merge([root])` instead to add
private roots while retaining the selected verifier's default trust. Do not disable
certificate validation to work around a trust-store difference. The
[proxy-header constraint](#proxy-headers) applies when explicitly configuring proxies.
