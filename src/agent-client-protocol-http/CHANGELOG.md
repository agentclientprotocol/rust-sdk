# Changelog

## [Unreleased]

### Curated release notes

- **Breaking:** Upgrade to `agent-client-protocol` 2.x. Transport implementations and the core
  handlers/types they connect must be migrated together.
- **Fixed:** Preserve incoming JSON-RPC batch frames and grouped responses across HTTP and WebSocket
  transports, including session-aware HTTP routing.
- **Fixed:** Keep call-bearing and invalid-request batch POSTs in peer order while allowing
  response-only frames, including malformed response-shaped values, to bypass a
  pending request when completing an SSE callback.
- **Fixed:** Allow an initial HTTP batch whose first call-shaped entry is `initialize`
  to create a connection and return its success or rejection as one grouped
  JSON-RPC response, ignoring any leading response-only entries and buffering
  sibling side traffic for the connection stream until initialization completes.

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
