# Agent Client Protocol Rust SDK

This repository contains Rust implementations of the Agent Client Protocol (ACP).

## What's in this SDK

This SDK includes two implementations:

### Legacy ACP SDK (`agent-client-protocol` crate)

The original Agent Client Protocol implementation. This is a stable, production-ready SDK that implements the base ACP specification. See the [Legacy SDK](#legacy-sdk) section for documentation.

### Experimental SACP Crates (`sacp-*` crates)

The Symposium extensions to ACP (SACP) are an experimental set of crates that explore advanced ACP features including:
- Proxy chains and composition
- MCP integration
- Enhanced component architecture

These crates will eventually become `agent-client-protocol` 1.0. See the [SACP Crates](#sacp-crates) section for documentation.

## Getting Started

For new projects, we recommend starting with the legacy SDK for production use, or experimenting with the SACP crates if you need advanced proxy features or want to help shape the future of the protocol.
