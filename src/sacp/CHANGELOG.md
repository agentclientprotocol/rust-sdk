# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0](https://github.com/symposium-dev/symposium-acp/compare/sacp-v1.0.0-alpha.7...sacp-v1.0.0) - 2025-11-13

### Fixed

- fix docs to not mention `Deref` impl

### Other

- Revert to state before 1.0.0 release
- release version 1.0.0 for all crates
- Merge pull request #32 from nikomatsakis/main

## [1.0.0-alpha.7](https://github.com/symposium-dev/symposium-acp/compare/sacp-v1.0.0-alpha.6...sacp-v1.0.0-alpha.7) - 2025-11-12

### Other

- Merge pull request #30 from nikomatsakis/main
- _(sacp)_ remove Deref impl from JrRequestCx
- _(sacp)_ add common patterns section to crate-level documentation
- _(sacp)_ add Component::serve() and simplify channel API

## [1.0.0-alpha.6](https://github.com/symposium-dev/symposium-acp/compare/sacp-v1.0.0-alpha.5...sacp-v1.0.0-alpha.6) - 2025-11-12

### Added

- _(sacp)_ extend IntoHandled support to notification handlers
- _(sacp)_ add IntoHandled trait for flexible handler return types

### Other

- Merge pull request #28 from nikomatsakis/main
- _(sacp)_ add tests for IntoHandled message transformation

## [1.0.0-alpha.5](https://github.com/symposium-dev/symposium-acp/compare/sacp-v1.0.0-alpha.4...sacp-v1.0.0-alpha.5) - 2025-11-11

### Other

- [**breaking**] make Component trait ergonomic with async fn and introduce DynComponent
- clarify that Component should be implemented instead of Transport
- [**breaking**] make Component the primary trait with Transport as blanket impl

## [1.0.0-alpha.4](https://github.com/symposium-dev/symposium-acp/compare/sacp-v1.0.0-alpha.3...sacp-v1.0.0-alpha.4) - 2025-11-11

### Other

- unify Transport and Component traits with BoxFuture-returning signatures
- create selective jsonrpcmsg re-export module
- move Component trait to sacp-proxy crate
- _(sacp)_ improve IntoJrTransport and Component trait impls

## [1.0.0-alpha.3](https://github.com/symposium-dev/symposium-acp/compare/sacp-v1.0.0-alpha.2...sacp-v1.0.0-alpha.3) - 2025-11-09

### Other

- Merge pull request #18 from nikomatsakis/main
- _(sacp-conductor)_ route all message forwarding through central queue

## [1.0.0-alpha.2](https://github.com/symposium-dev/symposium-acp/compare/sacp-v1.0.0-alpha.1...sacp-v1.0.0-alpha.2) - 2025-11-08

### Added

- _(sacp)_ add convenience methods for common connection patterns
- _(sacp)_ add IntoJrConnectionTransport trait and ByteStreamTransport

### Other

- fix doctests for API refactoring
- wip wip wip
- wipwipwip
- introduce a `IntoJrHandler` trait
- [**breaking**] remove Unpin bounds and simplify transport API
- _(sacp)_ clarify id: None semantics and remove phase references
- _(sacp)_ split actors into protocol and transport layers

## [0.2.0](https://github.com/symposium-dev/symposium-acp/compare/sacp-v0.1.1...sacp-v0.2.0) - 2025-11-04

### Added

- _(sacp-tokio)_ implement JrConnectionExt trait for to_agent
- create sacp-tokio crate and improve AcpAgent API
- _(sacp)_ add AcpAgent utility and yolo-one-shot-client example

### Fixed

- fix github url

### Other

- add GitHub links to example files in sacp and sacp-proxy
- _(sacp)_ [**breaking**] rename json_rpc_cx to connection_cx
- add deny(missing_docs) and document all public APIs
- _(sacp)_ improve cx cloning pattern in doc examples
- _(sacp)_ update crate-level documentation to match README
- factor "doc-test-only" code into its own crate
- make doctests build
- _(sacp)_ fix remaining doc test compilation errors
- _(sacp)_ enhance JrResponse method documentation
- _(sacp)_ document event loop and concurrency model
- rename JsonRpcRequest to JrRequest
- use util.rs
- _(sacp)_ move typed utilities to util module and add docs
- _(sacp)_ remove mention of non-existent derive macro
- _(sacp)_ fix doctests to compile instead of being ignored
- _(sacp)_ add comprehensive rustdoc for JrConnection
- _(sacp)_ rewrite README with streamlined quick start
- _(sacp)_ use stderr for yolo-one-shot-client meta output

## [0.1.1](https://github.com/symposium-dev/symposium-acp/compare/sacp-v0.1.0...sacp-v0.1.1) - 2025-10-30

### Added

- _(sacp)_ re-export all agent-client-protocol-schema types

### Fixed

- replace crate::Error with sacp::Error in dependent crates

### Other

- remove more uses of `agent_client_protocol_schema`
- replace acp:: with crate::/sacp:: throughout codebase
- rename JsonRpc* types to Jr* across all crates
- _(deps)_ switch from agent-client-protocol to agent-client-protocol-schema
- _(sacp)_ add simple_agent example demonstrating JsonRpcConnection usage
