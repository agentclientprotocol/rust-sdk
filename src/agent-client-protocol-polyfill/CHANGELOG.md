# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-polyfill-v1.3.0...agent-client-protocol-polyfill-v2.0.0) - 2026-07-23

### Breaking changes

- Upgrade to `agent-client-protocol` 2.x. Polyfill components and the core
  handlers/types they connect must be migrated together.
- Consume native `McpServer::Acp` declarations and route `mcp/connect`,
  `mcp/message`, and request/response `mcp/disconnect`. The bridge no longer recognizes
  `McpServer::Http` values with `acp:` URLs or the SDK-local underscore-prefixed method family.
  ([#281](https://github.com/agentclientprotocol/rust-sdk/pull/281))
- Remove the public `BridgeMode` enum and `McpOverAcpPolyfill::stdio`; the repository
  no longer ships the conductor helper subcommand that stdio mode required. The polyfill now has
  one supported configuration, selected with `McpOverAcpPolyfill::http()` or `Default`; otherwise
  use a separately managed MCP transport. ([#280](https://github.com/agentclientprotocol/rust-sdk/pull/280))

See the [core 2.0 migration guide](https://agentclientprotocol.github.io/rust-sdk/migration_v2.0.html)
for the shared transport and handler API changes.

### Changed

- Pass native MCP-over-ACP declarations through when the downstream agent supports them, translate
  them to localhost HTTP when the agent supports only HTTP MCP, and reject them when the agent
  supports neither transport. Apply the same behavior to new, load, resume, and feature-gated fork
  session setup; non-native declarations remain unchanged.
  ([#281](https://github.com/agentclientprotocol/rust-sdk/pull/281))

### Fixed

- Preserve JSON-RPC batch frames through the MCP-over-ACP HTTP bridge, answer
  malformed calls on their originating POST, and ignore malformed response-shaped input.
  ([#275](https://github.com/agentclientprotocol/rust-sdk/pull/275),
  [#280](https://github.com/agentclientprotocol/rust-sdk/pull/280))
- Process HTTP POSTs whose responses cannot be uniquely correlated by request ID
  sequentially—including overlapping IDs, `id: null`, and response-bearing batches without
  identifiable request IDs—and retain correlation state after a caller disconnects so late
  responses cannot reach the wrong POST.
  ([#280](https://github.com/agentclientprotocol/rust-sdk/pull/280))

## [1.0.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-polyfill-v1.0.0...agent-client-protocol-polyfill-v1.0.1) - 2026-06-29

### Other

- release v1.0.0 ([#226](https://github.com/agentclientprotocol/rust-sdk/pull/226))

## [1.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-polyfill-v0.15.1...agent-client-protocol-polyfill-v1.0.0) - 2026-06-24

### Other

- release ([#216](https://github.com/agentclientprotocol/rust-sdk/pull/216))

## [0.15.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-polyfill-v0.14.0...agent-client-protocol-polyfill-v0.15.0) - 2026-06-18

### Added

- *(deps)* update schema to 0.14.0 ([#211](https://github.com/agentclientprotocol/rust-sdk/pull/211))
- *(transports)* add HTTP/WebSocket transport support ([#162](https://github.com/agentclientprotocol/rust-sdk/pull/162))

### Other

- *(acp)* Replace jsonrpcmsg crate with shared schema types ([#205](https://github.com/agentclientprotocol/rust-sdk/pull/205))

## [0.14.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-polyfill-v0.13.1...agent-client-protocol-polyfill-v0.14.0) - 2026-06-05

### Other

- release v0.13.1 ([#189](https://github.com/agentclientprotocol/rust-sdk/pull/189))

## [0.13.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-polyfill-v0.12.2...agent-client-protocol-polyfill-v0.13.1) - 2026-06-01

### Other

- release ([#187](https://github.com/agentclientprotocol/rust-sdk/pull/187))

## [0.12.2](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-polyfill-v0.12.1...agent-client-protocol-polyfill-v0.12.2) - 2026-06-01

### Other

- updated the following local packages: agent-client-protocol

## [0.12.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-polyfill-v0.12.0...agent-client-protocol-polyfill-v0.12.1) - 2026-05-17

### Other

- updated the following local packages: agent-client-protocol

## [0.12.0](https://github.com/agentclientprotocol/rust-sdk/releases/tag/agent-client-protocol-polyfill-v0.12.0) - 2026-05-16

### Added

- extract mcp-over-acp proxy ([#146](https://github.com/agentclientprotocol/rust-sdk/pull/146))
