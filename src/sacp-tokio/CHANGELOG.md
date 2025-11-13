# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0](https://github.com/symposium-dev/symposium-acp/compare/sacp-tokio-v1.0.0-alpha.8...sacp-tokio-v1.0.0) - 2025-11-13

### Other

- updated the following local packages: sacp

## [1.0.0-alpha.8](https://github.com/symposium-dev/symposium-acp/compare/sacp-tokio-v1.0.0-alpha.7...sacp-tokio-v1.0.0-alpha.8) - 2025-11-12

### Other

- Merge pull request #30 from nikomatsakis/main
- *(sacp)* add Component::serve() and simplify channel API

## [1.0.0-alpha.7](https://github.com/symposium-dev/symposium-acp/compare/sacp-tokio-v1.0.0-alpha.6...sacp-tokio-v1.0.0-alpha.7) - 2025-11-12

### Other

- Merge pull request #28 from nikomatsakis/main

## [1.0.0-alpha.6](https://github.com/symposium-dev/symposium-acp/compare/sacp-tokio-v1.0.0-alpha.5...sacp-tokio-v1.0.0-alpha.6) - 2025-11-11

### Other

- Merge pull request #26 from nikomatsakis/main
- [**breaking**] make Component trait ergonomic with async fn and introduce DynComponent
- [**breaking**] make Component the primary trait with Transport as blanket impl

## [1.0.0-alpha.5](https://github.com/symposium-dev/symposium-acp/compare/sacp-tokio-v1.0.0-alpha.4...sacp-tokio-v1.0.0-alpha.5) - 2025-11-11

### Other

- convert Stdio to unit struct for easier reference

## [1.0.0-alpha.4](https://github.com/symposium-dev/symposium-acp/compare/sacp-tokio-v1.0.0-alpha.3...sacp-tokio-v1.0.0-alpha.4) - 2025-11-11

### Other

- remove ComponentProvider trait
- unify Transport and Component traits with BoxFuture-returning signatures
- create selective jsonrpcmsg re-export module
- replace jsonrpcmsg::Message with sacp::JsonRpcMessage throughout codebase

## [1.0.0-alpha.3](https://github.com/symposium-dev/symposium-acp/compare/sacp-tokio-v1.0.0-alpha.2...sacp-tokio-v1.0.0-alpha.3) - 2025-11-09

### Other

- updated the following local packages: sacp

## [1.0.0-alpha.2](https://github.com/symposium-dev/symposium-acp/compare/sacp-tokio-v1.0.0-alpha.1...sacp-tokio-v1.0.0-alpha.2) - 2025-11-08

### Other

- fix doctests for API refactoring
- wip wip wip
- [**breaking**] remove Unpin bounds and simplify transport API

## [1.0.0-alpha](https://github.com/symposium-dev/symposium-acp/releases/tag/sacp-tokio-v1.0.0-alpha) - 2025-11-05

### Added

- *(conductor)* add proxy mode support for hierarchical chains
- *(sacp-tokio)* implement JrConnectionExt trait for to_agent
- create sacp-tokio crate and improve AcpAgent API

### Fixed

- *(sacp-tokio)* correct type path in doctest example
- fix github url

### Other

- bump all packages to version 1.0.0-alpha
- *(sacp)* move handler types to dedicated handler module
- *(sacp)* [**breaking**] reorganize modules with flat schema namespace
- release
- add READMEs for sacp-tokio, sacp-proxy, and sacp-conductor

## [0.1.1](https://github.com/symposium-dev/symposium-acp/releases/tag/sacp-tokio-v0.1.1) - 2025-11-04

### Added

- *(sacp-tokio)* implement JrConnectionExt trait for to_agent
- create sacp-tokio crate and improve AcpAgent API

### Fixed

- fix github url

### Other

- add READMEs for sacp-tokio, sacp-proxy, and sacp-conductor
