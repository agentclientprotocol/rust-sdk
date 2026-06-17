# agent-client-protocol-http

HTTP/WebSocket transport for ACP agents.

- **Server**: `AcpHttpServer` exposes agents over HTTP + SSE with optional WebSocket upgrade
- **Client**: `HttpClient` connects to remote agents over HTTP + SSE

Cross-origin browser access is disabled by default. Configure `ServerOptions`
with `CorsOptions::allow_origins(...)` to allow specific browser origins.

Enable the `unstable_cancel_request` feature to forward core SDK request
cancellation support through this transport.

See the [documentation](https://docs.rs/agent-client-protocol-http) for usage examples.
