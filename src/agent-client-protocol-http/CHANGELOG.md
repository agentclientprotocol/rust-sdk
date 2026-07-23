# Changelog

## [Unreleased]

## [2.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-http-v1.3.0...agent-client-protocol-http-v2.0.0) - 2026-07-23

### Fixed

- *(http)* preserve transport lifecycle ordering ([#286](https://github.com/agentclientprotocol/rust-sdk/pull/286))
- [**breaking**] harden the 2.0 transport and API boundary ([#280](https://github.com/agentclientprotocol/rust-sdk/pull/280))
- *(acp)* [**breaking**] preserve JSON-RPC frames across transport adapters ([#275](https://github.com/agentclientprotocol/rust-sdk/pull/275))

### Curated release notes

- **Breaking:** Upgrade to `agent-client-protocol` 2.x. Transport implementations and the core
  handlers/types they connect must be migrated together.
- **Fixed:** Preserve incoming JSON-RPC batch frames and grouped responses across HTTP and WebSocket
  transports, including session-aware HTTP routing. HTTP clients now validate and track every batch
  entry and open streams returned by grouped `session/new` and `session/fork` responses.
- **Fixed:** Establish connection and session SSE streams before posting dependent messages while
  continuing to deliver callbacks and complete earlier POSTs during setup. Pending setup is
  cancelled cleanly when the caller exits.
- **Fixed:** Keep call-bearing and invalid-request batch POSTs in peer order while allowing
  response-only frames, including malformed response-shaped values, to bypass a
  pending request when completing an SSE callback.
- **Fixed:** Allow an initial HTTP batch whose first call-shaped entry is `initialize`
  to create a connection and return its success or rejection as one grouped
  JSON-RPC response, ignoring any leading response-only entries and buffering
  sibling side traffic for the connection stream until initialization completes.
- **Fixed:** Drain final routed messages to established HTTP SSE and WebSocket streams before
  closing them when the connected agent exits.

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
