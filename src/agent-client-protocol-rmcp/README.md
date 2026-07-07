# agent-client-protocol-rmcp

[rmcp](https://docs.rs/rmcp) integration for [Agent Client Protocol](https://agentclientprotocol.com/) MCP servers.

## Overview

This crate bridges [rmcp](https://docs.rs/rmcp)-based MCP server implementations with the ACP MCP server framework from `agent-client-protocol`. It lets you define MCP tools in Rust or use any rmcp service as an MCP server in an ACP proxy.

## Usage

Use the `McpServerExt` trait to build an MCP server with tools:

```rust
use agent_client_protocol::mcp_server::McpServer;
use agent_client_protocol_rmcp::McpServerExt;

let server = McpServer::builder("my-tools").build();
```

Or create an MCP server from an rmcp service:

```rust
use agent_client_protocol::mcp_server::McpServer;
use agent_client_protocol_rmcp::McpServerExt;

let server = McpServer::from_rmcp("my-server", MyRmcpService::new);

// Use as a handler in a proxy
Proxy.builder()
    .with_mcp_server(server)
    .connect_to(transport)
    .await?;
```

## Why a Separate Crate?

This crate is separate from `agent-client-protocol` to avoid coupling the core protocol crate to the `rmcp` dependency. This allows:

- `agent-client-protocol` to remain focused on the ACP protocol
- `agent-client-protocol-rmcp` to track `rmcp` updates independently
- Breaking changes in `rmcp` only require updating this crate

## Versioning

`rmcp` is a public dependency of this crate: its types appear in the public API (e.g. `McpServerExt::from_rmcp`). Each major release of `rmcp` therefore requires a major release of this crate, independent of the other `agent-client-protocol` crates. The crate re-exports the `rmcp` version it was built against as `agent_client_protocol_rmcp::rmcp` — prefer it over a direct `rmcp` dependency to guarantee matching versions.

| agent-client-protocol-rmcp | rmcp |
| -------------------------- | ---- |
| 2.x                        | 2.x  |
| 1.x                        | 1.x  |

## Related Crates

- **[agent-client-protocol](../agent-client-protocol/)** — Core ACP protocol types and traits
- **[agent-client-protocol-tokio](../agent-client-protocol-tokio/)** — Tokio utilities for spawning agent processes

## License

Apache-2.0
