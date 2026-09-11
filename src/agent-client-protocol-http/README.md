# agent-client-protocol-http

HTTP/WebSocket transport for ACP agents.

- **Server**: `AcpHttpServer` exposes agents over HTTP + SSE with optional WebSocket upgrade
- **Client**: `HttpClient` connects over HTTP + SSE or WebSocket, selected by the URL scheme

The crate does not enable either transport side by default. Opt into the
surface you need:

```toml
agent-client-protocol-http = { version = "...", features = ["client"] }
agent-client-protocol-http = { version = "...", features = ["server"] }
```

Cross-origin browser access is disabled by default. Configure `ServerOptions`
with `CorsOptions::allow_origins(...)` to allow specific browser origins.

Core SDK request cancellation support is forwarded through this transport.

Use `HttpClient::builder(url).configure_http(|http| ...).build()` to customize
reqwest headers, TLS, proxies, DNS, and timeouts. WebSocket handshakes use HTTP/1.1,
do not follow redirects, and are validated before sending ACP data. A raw
preconfigured TLS backend must itself use HTTP/1.1 ALPN.

`HttpClient::from_http_client(exact_endpoint, client)` reuses an existing reqwest
client for HTTP/SSE only. The old `with_client` and `with_endpoint_and_client`
constructors remain as deprecated HTTP/SSE compatibility wrappers. WebSocket
calls to those constructors return a migration error; use the builder instead.

See the [client configuration and migration guide](https://agentclientprotocol.github.io/rust-sdk/http-transport.html#client-configuration).

See the [documentation](https://docs.rs/agent-client-protocol-http) for usage examples.
