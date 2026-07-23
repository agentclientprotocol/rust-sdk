# Changelog

## [Unreleased]

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
