# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-derive-v1.3.0...agent-client-protocol-derive-v2.0.0) - 2026-07-23

### Fixed

- [**breaking**] harden the 2.0 transport and API boundary ([#280](https://github.com/agentclientprotocol/rust-sdk/pull/280))

### Other

- *(deps)* bump syn from 2.0.119 to 3.0.2 ([#267](https://github.com/agentclientprotocol/rust-sdk/pull/267))

### Curated 2.0 release notes

**Added**

- Support generic request, notification, and response types in the JSON-RPC derive macros.
- Accept any Rust type expression, including generic types such as `Option<Response>`, in a
  request's `response` attribute.

**Changed**

- Align this release line with `agent-client-protocol` 2.x; generated implementations target the
  corresponding core API.
- Update the macro parser to `syn` 3 and generate collision-resistant internal identifiers and
  fully qualified support-crate paths.

## [1.0.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-derive-v1.0.0...agent-client-protocol-derive-v1.0.1) - 2026-06-29

### Other

- release v1.0.0 ([#226](https://github.com/agentclientprotocol/rust-sdk/pull/226))

## [1.0.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-derive-v0.15.1...agent-client-protocol-derive-v1.0.0) - 2026-06-24

### Other

- release ([#216](https://github.com/agentclientprotocol/rust-sdk/pull/216))

## [0.14.0](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-derive-v0.13.1...agent-client-protocol-derive-v0.14.0) - 2026-06-05

### Other

- release v0.13.1 ([#189](https://github.com/agentclientprotocol/rust-sdk/pull/189))

## [0.13.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-derive-v0.11.1...agent-client-protocol-derive-v0.13.1) - 2026-06-01

### Other

- release ([#187](https://github.com/agentclientprotocol/rust-sdk/pull/187))

## [0.11.1](https://github.com/agentclientprotocol/rust-sdk/compare/agent-client-protocol-derive-v0.11.0...agent-client-protocol-derive-v0.11.1) - 2026-05-16

### Other

- Trim dependencies ([#149](https://github.com/agentclientprotocol/rust-sdk/pull/149))

## [0.11.0](https://github.com/agentclientprotocol/rust-sdk/releases/tag/agent-client-protocol-derive-v0.11.0) - 2026-04-20

### Added

- Migrate to new SDK design ([#117](https://github.com/agentclientprotocol/rust-sdk/pull/117))
- Bring in SACP crates again ([#102](https://github.com/agentclientprotocol/rust-sdk/pull/102))

### Fixed

- Catch handler errors instead of killing the connection ([#131](https://github.com/agentclientprotocol/rust-sdk/pull/131)) ([#114](https://github.com/agentclientprotocol/rust-sdk/pull/114))

### Other

- Add migration guide for next release ([#111](https://github.com/agentclientprotocol/rust-sdk/pull/111))
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
