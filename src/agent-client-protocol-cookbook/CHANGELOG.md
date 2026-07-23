# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-cookbook-v1.3.0...agent-client-protocol-cookbook-v2.0.0) - 2026-07-23

### Changed

- Update the cookbook for `agent-client-protocol` 2.x, including notification handling, ordered
  response callbacks, permission and streaming flows, feature-gated native MCP-over-ACP, and
  current conductor APIs. See the
  [2.0 migration guide](https://agentclientprotocol.github.io/rust-sdk/migration_v2.0.html).

## [1.2.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-cookbook-v1.1.0...agent-client-protocol-cookbook-v1.2.0) - 2026-07-07

### Added

- *(deps)* bump rmcp from 1.8.0 to 2.1.0 ([#239](https://github.com/agentclientprotocol/rust-sdk/pull/239))

## [1.0.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-cookbook-v1.0.0...agent-client-protocol-cookbook-v1.0.1) - 2026-06-29

### Other

- release v1.0.0 ([#226](https://github.com/agentclientprotocol/rust-sdk/pull/226))

## [1.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-cookbook-v0.15.1...agent-client-protocol-cookbook-v1.0.0) - 2026-06-24

### Other

- release ([#216](https://github.com/agentclientprotocol/rust-sdk/pull/216))

## [0.15.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-cookbook-v0.14.0...agent-client-protocol-cookbook-v0.15.0) - 2026-06-18

### Added

- *(deps)* update schema to 0.14.0 ([#211](https://github.com/agentclientprotocol/rust-sdk/pull/211))

## [0.14.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-cookbook-v0.13.1...agent-client-protocol-cookbook-v0.14.0) - 2026-06-05

### Other

- release v0.13.1 ([#189](https://github.com/agentclientprotocol/rust-sdk/pull/189))

## [0.13.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-cookbook-v0.11.3...agent-client-protocol-cookbook-v0.13.1) - 2026-06-01

### Other

- release ([#187](https://github.com/agentclientprotocol/rust-sdk/pull/187))

## [0.11.3](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-cookbook-v0.11.2...agent-client-protocol-cookbook-v0.11.3) - 2026-06-01

### Added

- *(acp)* Extract all rmcp logic to the rmcp crate ([#180](https://github.com/agentclientprotocol/rust-sdk/pull/180))

## [0.11.2](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-cookbook-v0.11.1...agent-client-protocol-cookbook-v0.11.2) - 2026-05-16

### Added

- remove direct dependency on tokio  ([#145](https://github.com/agentclientprotocol/rust-sdk/pull/145))

### Other

- Trim dependencies ([#149](https://github.com/agentclientprotocol/rust-sdk/pull/149))

## [0.11.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-cookbook-v0.11.0...agent-client-protocol-cookbook-v0.11.1) - 2026-04-21

### Other

- updated the following local packages: agent-client-protocol, agent-client-protocol-tokio, agent-client-protocol-conductor, agent-client-protocol-rmcp

## [0.11.0](https://github.com/agentclientprotocol/rust-sdk/releases/tag/agent-client-protocol-cookbook-v0.11.0) - 2026-04-20

### Added

- Migrate to new SDK design ([#117](https://github.com/agentclientprotocol/rust-sdk/pull/117))
- Bring in SACP crates again ([#102](https://github.com/agentclientprotocol/rust-sdk/pull/102))

### Fixed

- Re-export Result and update docs to use V1 ([#110](https://github.com/agentclientprotocol/rust-sdk/pull/110))

### Other

- Add migration guide for next release ([#111](https://github.com/agentclientprotocol/rust-sdk/pull/111))
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
