# Changelog

## [Unreleased]

## [3.3.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-http-v3.2.0...agent-client-protocol-http-v3.3.0) - 2026-10-09

### Other

- release ([#410](https://github.com/agentclientprotocol/rust-sdk/pull/410))

## [3.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-http-v2.2.0...agent-client-protocol-http-v3.0.0) - 2026-10-06

### Added

- *(config)* [**breaking**] make public configuration APIs extensible ([#403](https://github.com/agentclientprotocol/rust-sdk/pull/403))

### Fixed

- *(http)* [**breaking**] honor custom configuration with safe WebSocket handshakes ([#333](https://github.com/agentclientprotocol/rust-sdk/pull/333))
- avoid sensitive transport logs and redact trace URL credentials ([#401](https://github.com/agentclientprotocol/rust-sdk/pull/401))
- *(http)* gracefully finish client transports after builder completion ([#396](https://github.com/agentclientprotocol/rust-sdk/pull/396))
- *(acp)* [**breaking**] preserve connection lifetimes and graceful drain ([#385](https://github.com/agentclientprotocol/rust-sdk/pull/385))
- *(acp)* [**breaking**] preserve protocol-neutral raw JSON-RPC errors ([#384](https://github.com/agentclientprotocol/rust-sdk/pull/384))

### Other

- prepare v3 package metadata and release guidance ([#404](https://github.com/agentclientprotocol/rust-sdk/pull/404))
- slim dependency features and CI validation ([#399](https://github.com/agentclientprotocol/rust-sdk/pull/399))

### Breaking changes

- WebSocket URLs passed to `HttpClient::with_client` or
  `with_endpoint_and_client` now return `WebSocketRequiresBuilder` before network
  I/O. Migrate to `builder(...).configure_http(...).build()` or
  `builder_with_endpoint(...)` so the transport can enforce connection policies.
  See the
  [migration guide](https://agentclientprotocol.github.io/rust-sdk/http-transport.html#migrating-custom-client-construction).
- Unconfigured WebSockets now share reqwest's proxy discovery and TLS verification
  defaults, replacing direct connections and bundled WebPKI roots. Environment
  proxies, enabled system-proxy discovery, and platform certificate verification
  can change routing and trust. Use `no_proxy()` and explicit `tls_certs_only`
  roots when those defaults are not appropriate.

### Deprecated

- Retain `HttpClient::with_client` and `with_endpoint_and_client` as deprecated
  HTTP/SSE compatibility wrappers with unchanged path handling.
  `from_http_client(exact_endpoint, client)` preserves shared reqwest clients for
  HTTP/SSE; its endpoint is exact and does not append `/acp`.

### Changed

- Document the caller constraint against setting reserved WebSocket handshake
  headers through `reqwest::Proxy::headers`. Plain-WS proxy headers can overwrite
  SDK request headers after assembly; opaque proxy configuration cannot be
  inspected or rejected at build time. Cover normal plain-WS proxy headers and
  fail-closed accept validation after a proxy key override.
- **Breaking:** make `ServerOptions` and `CorsOptions` non-exhaustive.
  Replace server option literals (including struct update syntax) with
  `ServerOptions::default().with_path(...).with_cors(...).with_health_endpoint(...)`;
  existing fields remain public. Downstream CORS matches need a wildcard arm.
  Defaults and runtime policies are unchanged. See the
  [config API migration guide](../../md/migration-config-api.md).
- Limit Axum to HTTP1/Tokio, WebSockets, and tracing/tower-log observability.
  Remove unused Axum macros and extractors; JSON extraction is enabled only for
  tests. Drop unused test-utility and tracing-subscriber dev dependencies and
  the unused production futures executor feature.
- Adapt `HttpClient`'s `ConnectTo` conversion to the core SDK's breaking
  optional `ConnectionDriver` return type. Channels and HTTP framing remain
  unchanged; no new resource limits are introduced.

### Fixed

- Apply custom headers, TLS, proxies, DNS, and timeouts to WebSocket handshakes.
  Enforce HTTP/1.1 and disable redirects for WebSockets without changing HTTP/SSE
  policies. Validate the upgrade response before sending queued ACP data, and
  reject unsupported subprotocols and extensions.
- Replace HTTP POST, SSE, and WebSocket payload diagnostics with bounded
  metadata. Do not log peer-controlled IDs, close reasons, or transport error
  text (which may include credentials or message content).
- Cooperatively finish HTTP client connections created through the public builder:
  drain accepted ordered POSTs and WebSocket frames before physical cleanup, reject
  output from escaped transport senders, and report transport failures during shutdown.
- Preserve raw JSON-RPC error codes, omitted versus null data, and error
  extension fields across HTTP/SSE and WebSocket transports, using the core
  SDK's new `RawJsonRpcResponse` representation.
- Preserve HTTP channel pumps when an agent-factory endpoint has no owned
  driver; absence is not agent completion while its transport remains open.
- On active agent completion, reject further output from escaped sender clones
  and drain accepted frames before removing the connection and closing its
  streams, without waiting for those clones to be dropped.
- Keep router cancellation owned while natural cleanup awaits its drain.
  Explicit shutdown no longer detaches a taken router task or retains the
  connection through that orphaned task.

## [2.2.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-http-v2.1.0...agent-client-protocol-http-v2.2.0) - 2026-09-18

### Other

- *(deps)* bump actions-rust-lang/setup-rust-toolchain from 1.17.0 to 2.0.0 ([#356](https://github.com/agentclientprotocol/rust-sdk/pull/356))

## [2.1.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-http-v2.0.0...agent-client-protocol-http-v2.1.0) - 2026-09-04

### Added

- *(unstable-v2)* Add runnable v2 quickstart examples ([#330](https://github.com/agentclientprotocol/rust-sdk/pull/330))

### Fixed

- *(http)* Preserve outbound messages for slow streams ([#292](https://github.com/agentclientprotocol/rust-sdk/pull/292))

## [2.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-http-v1.3.0...agent-client-protocol-http-v2.0.0) - 2026-07-23

### Breaking changes

- Upgrade to `agent-client-protocol` 2.x. Transport implementations and the core
  handlers/types they connect must be migrated together.

See the [core 2.0 migration guide](https://agentclientprotocol.github.io/rust-sdk/migration_v2.0.html)
for the shared transport changes.

### Fixed

- Preserve incoming JSON-RPC batch frames and grouped responses across HTTP and WebSocket
  transports, including session-aware HTTP routing. HTTP clients validate batch messages, track
  session and request bookkeeping for valid entries, and open streams returned by grouped
  `session/new` and `session/fork` responses. The HTTP server accepts a mixed initial batch when
  `initialize` is its first call-shaped entry.
  ([#275](https://github.com/agentclientprotocol/rust-sdk/pull/275),
  [#280](https://github.com/agentclientprotocol/rust-sdk/pull/280),
  [#286](https://github.com/agentclientprotocol/rust-sdk/pull/286))
- Establish connection and session SSE streams before posting dependent messages while
  continuing to deliver callbacks and complete earlier POSTs during setup. Pending setup is
  cancelled cleanly when the outgoing ACP channel closes. Call-bearing batches remain in peer
  order, while response-only frames can bypass a pending call to complete an SSE callback.
  ([#280](https://github.com/agentclientprotocol/rust-sdk/pull/280),
  [#286](https://github.com/agentclientprotocol/rust-sdk/pull/286))
- Drain final routed messages to established HTTP SSE and WebSocket streams before closing them
  when the connected agent exits. ([#286](https://github.com/agentclientprotocol/rust-sdk/pull/286))

## [1.3.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-http-v1.2.0...agent-client-protocol-http-v1.3.0) - 2026-07-20

### Fixed

- *(acp)* Handle  incoming EOF correctly ([#261](https://github.com/agentclientprotocol/rust-sdk/pull/261))

## [1.1.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-http-v1.0.1...agent-client-protocol-http-v1.1.0) - 2026-07-06

### Added

- *(acp)* Make request cancellation stable ([#242](https://github.com/agentclientprotocol/rust-sdk/pull/242))

## [1.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-http-v0.1.1...agent-client-protocol-http-v1.0.0) - 2026-06-24

### Other

- *(deps)* bump tower-http from 0.6.11 to 0.7.0 ([#220](https://github.com/agentclientprotocol/rust-sdk/pull/220))

## [0.1.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-http-v0.1.0...agent-client-protocol-http-v0.1.1) - 2026-06-22

### Other

- updated the following local packages: agent-client-protocol

## [0.1.0](https://github.com/agentclientprotocol/rust-sdk/releases/tag/agent-client-protocol-http-v0.1.0) - 2026-06-18

### Added

- *(deps)* update schema to 0.14.0 ([#211](https://github.com/agentclientprotocol/rust-sdk/pull/211))
- *(transports)* add HTTP/WebSocket transport support ([#162](https://github.com/agentclientprotocol/rust-sdk/pull/162))
