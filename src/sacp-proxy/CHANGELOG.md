# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0](https://github.com/symposium-dev/symposium-acp/compare/sacp-proxy-v1.0.0-alpha.6...sacp-proxy-v1.0.0) - 2025-11-13

### Other

- Merge pull request #30 from nikomatsakis/main
- *(sacp)* remove Deref impl from JrRequestCx

## [1.0.0-alpha.6](https://github.com/symposium-dev/symposium-acp/compare/sacp-proxy-v1.0.0-alpha.5...sacp-proxy-v1.0.0-alpha.6) - 2025-11-12

### Other

- Merge pull request #28 from nikomatsakis/main

## [1.0.0-alpha.5](https://github.com/symposium-dev/symposium-acp/compare/sacp-proxy-v1.0.0-alpha.4...sacp-proxy-v1.0.0-alpha.5) - 2025-11-11

### Other

- updated the following local packages: sacp

## [1.0.0-alpha.4](https://github.com/symposium-dev/symposium-acp/compare/sacp-proxy-v1.0.0-alpha.3...sacp-proxy-v1.0.0-alpha.4) - 2025-11-11

### Other

- unify Transport and Component traits with BoxFuture-returning signatures
- create selective jsonrpcmsg re-export module
- replace jsonrpcmsg::Message with sacp::JsonRpcMessage throughout codebase
- move Component trait to sacp-proxy crate

## [1.0.0-alpha.3](https://github.com/symposium-dev/symposium-acp/compare/sacp-proxy-v1.0.0-alpha.2...sacp-proxy-v1.0.0-alpha.3) - 2025-11-09

### Other

- updated the following local packages: sacp

## [1.0.0-alpha.2](https://github.com/symposium-dev/symposium-acp/compare/sacp-proxy-v1.0.0-alpha.1...sacp-proxy-v1.0.0-alpha.2) - 2025-11-08

### Other

- fix doctests for API refactoring
- wip wip wip
- wipwipwip
- introduce a `IntoJrHandler` trait
- [**breaking**] remove Unpin bounds and simplify transport API

## [0.1.2](https://github.com/symposium-dev/symposium-acp/compare/sacp-proxy-v0.1.1...sacp-proxy-v0.1.2) - 2025-11-04

### Fixed

- fix github url

### Other

- add GitHub links to example files in sacp and sacp-proxy
- add deny(missing_docs) and document all public APIs
- *(sacp-proxy)* add comprehensive crate-level documentation
- rename JsonRpcRequest to JrRequest
- add READMEs for sacp-tokio, sacp-proxy, and sacp-conductor

## [0.1.1](https://github.com/symposium-dev/symposium-acp/compare/sacp-proxy-v0.1.0...sacp-proxy-v0.1.1) - 2025-10-30

### Fixed

- replace crate::Error with sacp::Error in dependent crates

### Other

- remove more uses of `agent_client_protocl_schema`
- replace acp:: with crate::/sacp:: throughout codebase
- rename JsonRpc* types to Jr* across all crates
- *(deps)* switch from agent-client-protocol to agent-client-protocol-schema
