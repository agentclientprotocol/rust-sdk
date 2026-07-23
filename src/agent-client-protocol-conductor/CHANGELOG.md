# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-conductor-v1.3.0...agent-client-protocol-conductor-v2.0.0) - 2026-07-23

### Breaking changes

- Upgrade to `agent-client-protocol` 2.x. Conductor components and the core
  handlers/types they expose must be migrated together.
- Rename the public `ConductorResponder` background task to `ConductorRunner`, matching the core
  runner API it implements. ([#277](https://github.com/agentclientprotocol/rust-sdk/pull/277))

See the [core 2.0 migration guide](https://agentclientprotocol.github.io/rust-sdk/migration_v2.0.html)
for the shared API changes.

### Changed

- Enable the core SDK's draft native MCP-over-ACP support so traces classify `mcp/message`.
  HTTP translation remains an explicit `agent-client-protocol-polyfill` proxy.
  ([#281](https://github.com/agentclientprotocol/rust-sdk/pull/281))

### Fixed

- Preserve JSON-RPC batch framing when conductor tracing is enabled.
  ([#275](https://github.com/agentclientprotocol/rust-sdk/pull/275))

### Documentation

- Update examples for the current conductor and RMCP APIs, removing references to the retired MCP
  bridge CLI mode and `serve()` API. ([#287](https://github.com/agentclientprotocol/rust-sdk/pull/287))

## [1.3.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-conductor-v1.2.0...agent-client-protocol-conductor-v1.3.0) - 2026-07-20

### Other

- *(deps)* bump the minor group with 14 updates ([#258](https://github.com/agentclientprotocol/rust-sdk/pull/258))

## [1.2.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-conductor-v1.1.0...agent-client-protocol-conductor-v1.2.0) - 2026-07-07

### Added

- *(deps)* bump rmcp from 1.8.0 to 2.1.0 ([#239](https://github.com/agentclientprotocol/rust-sdk/pull/239))

## [1.1.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-conductor-v1.0.1...agent-client-protocol-conductor-v1.1.0) - 2026-07-06

### Added

- *(acp)* Make request cancellation stable ([#242](https://github.com/agentclientprotocol/rust-sdk/pull/242))

## [1.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-conductor-v0.15.1...agent-client-protocol-conductor-v1.0.0) - 2026-06-24

### Added

- *(test)* Expand scenarios testy can support ([#221](https://github.com/agentclientprotocol/rust-sdk/pull/221))

## [0.15.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-conductor-v0.14.0...agent-client-protocol-conductor-v0.15.0) - 2026-06-18

### Added

- *(deps)* update schema to 0.14.0 ([#211](https://github.com/agentclientprotocol/rust-sdk/pull/211))
- *(acp)* Update schema crate to 0.13.8 ([#210](https://github.com/agentclientprotocol/rust-sdk/pull/210))
- *(transports)* add HTTP/WebSocket transport support ([#162](https://github.com/agentclientprotocol/rust-sdk/pull/162))
- *(acp)* add unstable request cancellation support ([#179](https://github.com/agentclientprotocol/rust-sdk/pull/179))

### Other

- *(acp)* Replace jsonrpcmsg crate with shared schema types ([#205](https://github.com/agentclientprotocol/rust-sdk/pull/205))
- *(acp)* Remove unused module files ([#204](https://github.com/agentclientprotocol/rust-sdk/pull/204))
- *(deps)* Preserve serde_json object order ([#202](https://github.com/agentclientprotocol/rust-sdk/pull/202))

## [0.14.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-conductor-v0.13.1...agent-client-protocol-conductor-v0.14.0) - 2026-06-05

### Fixed

- *(acp)* Serialize proxy metadata as _meta ([#198](https://github.com/agentclientprotocol/rust-sdk/pull/198))

### Other

- Add features to docs.rs ([#190](https://github.com/agentclientprotocol/rust-sdk/pull/190))

## [0.13.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-conductor-v0.12.2...agent-client-protocol-conductor-v0.13.1) - 2026-06-01

### Other

- update Cargo.lock dependencies

## [0.12.2](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-conductor-v0.12.1...agent-client-protocol-conductor-v0.12.2) - 2026-06-01

### Added

- *(acp)* Extract all rmcp logic to the rmcp crate ([#180](https://github.com/agentclientprotocol/rust-sdk/pull/180))

## [0.12.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-conductor-v0.12.0...agent-client-protocol-conductor-v0.12.1) - 2026-05-17

### Fixed

- *(polyfill)* bump version to 0.12.0 ([#168](https://github.com/agentclientprotocol/rust-sdk/pull/168))

## [0.12.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-conductor-v0.11.1...agent-client-protocol-conductor-v0.12.0) - 2026-05-16

### Added

- extract mcp-over-acp proxy ([#146](https://github.com/agentclientprotocol/rust-sdk/pull/146))
- remove direct dependency on tokio  ([#145](https://github.com/agentclientprotocol/rust-sdk/pull/145))

### Other

- *(deps)* update Rust dependencies ([#166](https://github.com/agentclientprotocol/rust-sdk/pull/166))
- *(deps)* bump the minor group with 7 updates ([#152](https://github.com/agentclientprotocol/rust-sdk/pull/152))
- Trim dependencies ([#149](https://github.com/agentclientprotocol/rust-sdk/pull/149))
- remove unreachable!() and improve error messages ([#139](https://github.com/agentclientprotocol/rust-sdk/pull/139))

### Breaking Changes

- **Removed `McpBridgeMode`** and the `mcp_bridge_mode` parameter from `ConductorImpl::new`, `new_agent`, and `new_proxy`. MCP-over-ACP bridging is no longer built into the conductor. Use `agent-client-protocol-polyfill::mcp_over_acp::McpOverAcpPolyfill` as a proxy in the chain instead.
- **Removed `conductor mcp $port` CLI subcommand.** The stdio↔TCP bridge subprocess is no longer needed.

## [0.11.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-conductor-v0.11.0...agent-client-protocol-conductor-v0.11.1) - 2026-04-21

### Other

- updated the following local packages: agent-client-protocol, agent-client-protocol-tokio, agent-client-protocol-tokio

## [0.11.0](https://github.com/agentclientprotocol/rust-sdk/releases/tag/agent-client-protocol-conductor-v0.11.0) - 2026-04-20

### Added

- *(schema)* Update schema to 0.12.0 ([#119](https://github.com/agentclientprotocol/rust-sdk/pull/119))
- Migrate to new SDK design ([#117](https://github.com/agentclientprotocol/rust-sdk/pull/117))
- Bring in SACP crates again ([#102](https://github.com/agentclientprotocol/rust-sdk/pull/102))

### Fixed

- Remove redundant Box::pin calls from async code ([#106](https://github.com/agentclientprotocol/rust-sdk/pull/106))

### Other

- Cleanup docs still referencing sacp ([#129](https://github.com/agentclientprotocol/rust-sdk/pull/129))
- Add migration guide for next release ([#111](https://github.com/agentclientprotocol/rust-sdk/pull/111))
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
