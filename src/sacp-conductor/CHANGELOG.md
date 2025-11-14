# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0](https://github.com/symposium-dev/symposium-acp/compare/sacp-conductor-v1.0.0-alpha.9...sacp-conductor-v1.0.0) - 2025-11-13

### Other

- _(sacp-proxy)_ [**breaking**] move rmcp integration to separate sacp-rmcp crate

## [1.0.0-alpha.9](https://github.com/symposium-dev/symposium-acp/compare/sacp-conductor-v1.0.0-alpha.8...sacp-conductor-v1.0.0-alpha.9) - 2025-11-12

### Other

- Merge pull request #30 from nikomatsakis/main
- _(sacp)_ remove Deref impl from JrRequestCx
- _(sacp)_ add Component::serve() and simplify channel API

## [1.0.0-alpha.8](https://github.com/symposium-dev/symposium-acp/compare/sacp-conductor-v1.0.0-alpha.7...sacp-conductor-v1.0.0-alpha.8) - 2025-11-12

### Other

- Merge pull request #28 from nikomatsakis/main

## [1.0.0-alpha.7](https://github.com/symposium-dev/symposium-acp/compare/sacp-conductor-v1.0.0-alpha.6...sacp-conductor-v1.0.0-alpha.7) - 2025-11-11

### Other

- Merge pull request #26 from nikomatsakis/main
- [**breaking**] make Component trait ergonomic with async fn and introduce DynComponent
- [**breaking**] make Component the primary trait with Transport as blanket impl

## [1.0.0-alpha.6](https://github.com/symposium-dev/symposium-acp/compare/sacp-conductor-v1.0.0-alpha.5...sacp-conductor-v1.0.0-alpha.6) - 2025-11-11

### Added

- _(sacp-conductor)_ implement Component trait for Conductor
- _(sacp-conductor)_ add SYMPOSIUM_LOG environment variable for file logging

### Other

- Merge pull request #24 from nikomatsakis/main

## [1.0.0-alpha.5](https://github.com/symposium-dev/symposium-acp/compare/sacp-conductor-v1.0.0-alpha.4...sacp-conductor-v1.0.0-alpha.5) - 2025-11-11

### Other

- convert Stdio to unit struct for easier reference

## [1.0.0-alpha.4](https://github.com/symposium-dev/symposium-acp/compare/sacp-conductor-v1.0.0-alpha.3...sacp-conductor-v1.0.0-alpha.4) - 2025-11-11

### Other

- cleanup and simplify some of the logic to avoid "indirection" through
- remove ComponentProvider trait
- unify Transport and Component traits with BoxFuture-returning signatures
- create selective jsonrpcmsg re-export module
- replace jsonrpcmsg::Message with sacp::JsonRpcMessage throughout codebase
- move Component trait to sacp-proxy crate
- _(sacp)_ improve IntoJrTransport and Component trait impls
- _(sacp-conductor)_ make Component trait mirror IntoJrTransport interface
- _(sacp-conductor)_ introduce Component trait for better semantics

## [1.0.0-alpha.3](https://github.com/symposium-dev/symposium-acp/compare/sacp-conductor-v1.0.0-alpha.2...sacp-conductor-v1.0.0-alpha.3) - 2025-11-09

### Added

- _(sacp-conductor)_ implement lazy component initialization
- _(sacp-conductor)_ add ComponentList trait for lazy component instantiation

### Fixed

- _(sacp-conductor)_ prevent response messages from overtaking notifications

### Other

- Merge pull request #18 from nikomatsakis/main
- _(sacp-conductor)_ document lazy initialization and ComponentList trait
- _(sacp-conductor)_ use if-let and assert in lazy initialization
- _(sacp-conductor)_ route all message forwarding through central queue
- _(sacp-conductor)_ decouple message handler from component count

## [1.0.0-alpha.2](https://github.com/symposium-dev/symposium-acp/compare/sacp-conductor-v1.0.0-alpha.1...sacp-conductor-v1.0.0-alpha.2) - 2025-11-08

### Added

- _(sacp)_ add convenience methods for common connection patterns

### Other

- fix doctests for API refactoring
- wip wip wip
- wipwipwip
- [**breaking**] remove Unpin bounds and simplify transport API

## [0.2.0](https://github.com/symposium-dev/symposium-acp/compare/sacp-conductor-v0.1.1...sacp-conductor-v0.2.0) - 2025-11-04

### Fixed

- fix github url

### Other

- _(sacp-conductor)_ add comprehensive crate-level documentation
- _(sacp)_ [**breaking**] rename json_rpc_cx to connection_cx
- rename JsonRpcRequest to JrRequest
- add READMEs for sacp-tokio, sacp-proxy, and sacp-conductor

## [0.1.1](https://github.com/symposium-dev/symposium-acp/compare/sacp-conductor-v0.1.0...sacp-conductor-v0.1.1) - 2025-10-30

### Fixed

- replace crate::Error with sacp::Error in dependent crates

### Other

- remove more uses of `agent_client_protocol_schema`
- replace acp:: with crate::/sacp:: throughout codebase
- rename JsonRpc* types to Jr* across all crates
- _(deps)_ switch from agent-client-protocol to agent-client-protocol-schema
- prepare release-plz
