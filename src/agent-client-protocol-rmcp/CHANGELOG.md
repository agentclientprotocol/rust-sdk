# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [3.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v2.0.1...agent-client-protocol-rmcp-v3.0.0) - 2026-07-23

### Fixed

- [**breaking**] harden the 2.0 transport and API boundary ([#280](https://github.com/agentclientprotocol/rust-sdk/pull/280))

### Other

- update conductor and RMCP usage examples ([#287](https://github.com/agentclientprotocol/rust-sdk/pull/287))
- *(acp)* [**breaking**] adopt schema-native MCP-over-ACP ([#281](https://github.com/agentclientprotocol/rust-sdk/pull/281))
- *(acp)* clean up stale API terminology ([#279](https://github.com/agentclientprotocol/rust-sdk/pull/279))

### Curated release notes

- **Breaking change:** Upgrade the public `agent-client-protocol` dependency from 1.x to
  2.x. Downstream code using core SDK types through this crate must migrate both
  crates together; this requires `agent-client-protocol-rmcp` 3.0.0.
- **Changed:** Align `McpServerBuilder`'s background-task terminology with the core
  runner APIs.
- **Changed:** Keep rmcp-backed standalone MCP servers independent of the
  `unstable_mcp_over_acp` feature. Applications enable this crate's matching passthrough feature
  only when attaching a server to ACP. Attached services use native `McpServer::Acp`
  declarations, `mcp/connect`, `mcp/message`, and request/response `mcp/disconnect`; optional
  typed `server_id` and `connection_id` accessors replace `acp_id` in tool and connection
  contexts.
- **Documentation:** Replace removed handler and `serve()` APIs in examples and
  document compatibility with both public dependencies.

## [2.0.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v2.0.0...agent-client-protocol-rmcp-v2.0.1) - 2026-07-20

### Other

- updated the following local packages: agent-client-protocol

## [2.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v1.1.0...agent-client-protocol-rmcp-v2.0.0) - 2026-07-07

### Added

- [**breaking**] *(deps)* bump rmcp from 1.8.0 to 2.1.0 ([#239](https://github.com/agentclientprotocol/rust-sdk/pull/239)) — `rmcp` is a public dependency; its 2.x types (e.g. `ContentBlock`) appear in this crate's API

## [1.0.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v1.0.0...agent-client-protocol-rmcp-v1.0.1) - 2026-06-29

### Other

- release v1.0.0 ([#226](https://github.com/agentclientprotocol/rust-sdk/pull/226))

## [1.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v0.15.1...agent-client-protocol-rmcp-v1.0.0) - 2026-06-24

### Other

- release ([#216](https://github.com/agentclientprotocol/rust-sdk/pull/216))

## [0.15.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v0.14.0...agent-client-protocol-rmcp-v0.15.0) - 2026-06-18

### Added

- *(transports)* add HTTP/WebSocket transport support ([#162](https://github.com/agentclientprotocol/rust-sdk/pull/162))

## [0.14.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v0.13.1...agent-client-protocol-rmcp-v0.14.0) - 2026-06-05

### Other

- release v0.13.1 ([#189](https://github.com/agentclientprotocol/rust-sdk/pull/189))

## [0.13.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v0.11.3...agent-client-protocol-rmcp-v0.13.1) - 2026-06-01

### Other

- release ([#187](https://github.com/agentclientprotocol/rust-sdk/pull/187))

## [0.11.3](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v0.11.2...agent-client-protocol-rmcp-v0.11.3) - 2026-06-01

### Added

- *(acp)* Extract all rmcp logic to the rmcp crate ([#180](https://github.com/agentclientprotocol/rust-sdk/pull/180))

### Added

- Add the MCP server builder APIs moved out of `agent-client-protocol`, keeping `rmcp` and Tokio dependencies in this integration crate.

## [0.11.2](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v0.11.1...agent-client-protocol-rmcp-v0.11.2) - 2026-05-16

### Other

- Trim dependencies ([#149](https://github.com/agentclientprotocol/rust-sdk/pull/149))

## [0.11.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v0.11.0...agent-client-protocol-rmcp-v0.11.1) - 2026-04-21

### Other

- updated the following local packages: agent-client-protocol

## [0.11.0](https://github.com/agentclientprotocol/rust-sdk/releases/tag/agent-client-protocol-rmcp-v0.11.0) - 2026-04-20

### Added

- Migrate to new SDK design ([#117](https://github.com/agentclientprotocol/rust-sdk/pull/117))
- Bring in SACP crates again ([#102](https://github.com/agentclientprotocol/rust-sdk/pull/102))

### Fixed

- Remove redundant Box::pin calls from async code ([#106](https://github.com/agentclientprotocol/rust-sdk/pull/106))

### Other

- Fix dead code for release builds ([#118](https://github.com/agentclientprotocol/rust-sdk/pull/118))
- Add migration guide for next release ([#111](https://github.com/agentclientprotocol/rust-sdk/pull/111))
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
