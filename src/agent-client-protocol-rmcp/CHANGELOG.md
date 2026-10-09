# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [4.0.3](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v4.0.2...agent-client-protocol-rmcp-v4.0.3) - 2026-10-09

### Other

- updated the following local packages: agent-client-protocol

## [4.0.2](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v4.0.1...agent-client-protocol-rmcp-v4.0.2) - 2026-10-08

### Other

- updated the following local packages: agent-client-protocol

## [4.0.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v4.0.0...agent-client-protocol-rmcp-v4.0.1) - 2026-10-07

### Other

- updated the following local packages: agent-client-protocol

## [4.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v3.1.1...agent-client-protocol-rmcp-v4.0.0) - 2026-10-06

### Added

- *(acp)* [**breaking**] make native transports and schema support opt-in ([#397](https://github.com/agentclientprotocol/rust-sdk/pull/397))
- *(acp)* [**breaking**] adopt request-scoped MCP binding ([#388](https://github.com/agentclientprotocol/rust-sdk/pull/388))
- *(rmcp)* [**breaking**] upgrade integration to rmcp 3.4
- *(acp)* make schemars optional ([#373](https://github.com/agentclientprotocol/rust-sdk/pull/373))

### Other

- prepare v3 package metadata and release guidance ([#404](https://github.com/agentclientprotocol/rust-sdk/pull/404))
- slim dependency features and CI validation ([#399](https://github.com/agentclientprotocol/rust-sdk/pull/399))
- reconcile combined breaking release migrations ([#395](https://github.com/agentclientprotocol/rust-sdk/pull/395))

### Breaking changes

- Native attachments use request-scoped MCP 2026-07-28 and inner outcome
  carriers, replacing the old `mcp/connect` / `mcp/disconnect` binding without
  a legacy fallback. `from_rmcp` lazily creates one reusable service for native
  requests; standalone connections retain per-connection factories and rmcp
  protocol negotiation. See the
  [native binding migration](https://agentclientprotocol.github.io/rust-sdk/migration-stateless-mcp.html)
  and [request-scoped MCP guide](https://agentclientprotocol.github.io/rust-sdk/mcp-over-acp.html).
- Release `agent-client-protocol-rmcp` 4.x together with core ACP 3.x and the
  public `rmcp` dependency upgrade from 2.x to 3.4. Migrate both public
  dependencies together. Services supplied to `McpServer::from_rmcp` must use
  rmcp 3.x; custom transports and low-level callers must follow the core
  [connection-driver migration](https://agentclientprotocol.github.io/rust-sdk/migration-connection-drivers.html).
- Adapt tool handlers to rmcp's `CallToolResponse`, use `ServerConfig` in place
  of the deprecated `ServerInfo` alias, and remove legacy tool-execution
  metadata. See the [migration guide](https://agentclientprotocol.github.io/rust-sdk/migration-rmcp-v4.html).

### Added

- Integration coverage for MCP 2026-07-28 requests without initialization,
  discovery, per-request metadata/version validation, and MRTR results from
  caller-supplied rmcp services.
- Add supervised native execution, request-scoped cancellation/notification
  context, borrowed tool cleanup acknowledgments, and a runnable direct ACP
  example. Independent `unstable_protocol_v2` and `unstable_mcp_over_acp`
  features forward their respective core gates; the v2 native example
  explicitly requires both.

### Changed

- Request tokio-util's cancellation-token support explicitly for native MCP
  attachments. Keep production compatibility adapters and JSON Schema tool
  builders; remove unused JSON log formatting from example dependencies.
- Keep Tokio stdio, the multithreaded runtime, and rmcp macros out of production
  dependencies. Native examples and tests enable them separately. The integration
  library now compiles for `wasm32-wasip1` and `wasm32-wasip2` with default or all
  features; applications still supply a compatible runtime and host transport.

## [3.1.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v3.1.0...agent-client-protocol-rmcp-v3.1.1) - 2026-09-18

### Other

- *(deps)* bump actions-rust-lang/setup-rust-toolchain from 1.17.0 to 2.0.0 ([#356](https://github.com/agentclientprotocol/rust-sdk/pull/356))

## [3.1.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v3.0.0...agent-client-protocol-rmcp-v3.1.0) - 2026-09-04

### Added

- *(unstable-v2)* Add runnable v2 quickstart examples ([#330](https://github.com/agentclientprotocol/rust-sdk/pull/330))

## [3.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-rmcp-v2.0.1...agent-client-protocol-rmcp-v3.0.0) - 2026-07-23

### Breaking changes

- Use `agent-client-protocol-rmcp` 3.x with `agent-client-protocol` 2.x and `rmcp` 2.x. Both
  dependencies appear in this crate's public API and must be migrated together.
- Attached rmcp services now use native `McpServer::Acp` declarations, `mcp/connect`,
  `mcp/message`, and request/response `mcp/disconnect`; optional typed `server_id` and
  `connection_id` accessors replace `acp_id` in tool and connection contexts.
  ([#281](https://github.com/agentclientprotocol/rust-sdk/pull/281))

See the [core 2.0 migration guide](https://agentclientprotocol.github.io/rust-sdk/migration_v2.0.html)
for the shared API changes.

### Changed

- Keep rmcp-backed standalone MCP servers independent of the
  `unstable_mcp_over_acp` feature. Applications enable this crate's matching passthrough feature
  only when attaching a server to ACP. ([#281](https://github.com/agentclientprotocol/rust-sdk/pull/281))

### Documentation

- Replace removed handler and `serve()` APIs in examples and document compatibility with both
  public dependencies. ([#279](https://github.com/agentclientprotocol/rust-sdk/pull/279),
  [#287](https://github.com/agentclientprotocol/rust-sdk/pull/287))

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
