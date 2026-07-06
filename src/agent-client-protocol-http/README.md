# agent-client-protocol-http

HTTP/WebSocket transport for ACP agents.

- **Server**: `AcpHttpServer` exposes agents over HTTP + SSE with optional WebSocket upgrade
- **Client**: `HttpClient` connects to remote agents over HTTP + SSE

The crate does not enable either transport side by default. Opt into the
surface you need:

```toml
agent-client-protocol-http = { version = "...", features = ["client"] }
agent-client-protocol-http = { version = "...", features = ["server"] }
```

Cross-origin browser access is disabled by default. Configure `ServerOptions`
with `CorsOptions::allow_origins(...)` to allow specific browser origins.

Core SDK request cancellation support is forwarded through this transport.

See the [documentation](https://docs.rs/agent-client-protocol-http) for usage examples.
