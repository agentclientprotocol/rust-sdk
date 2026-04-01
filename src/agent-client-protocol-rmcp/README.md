# agent-client-protocol-rmcp

[rmcp](https://docs.rs/rmcp) integration for [Agent Client Protocol](https://agentclientprotocol.com/) MCP servers.

## Overview

This crate bridges [rmcp](https://docs.rs/rmcp)-based MCP server implementations with the ACP MCP server framework from `agent-client-protocol-core`. It lets you use any rmcp service as an MCP server in an ACP proxy.

## Usage

Use the `McpServerExt` trait to create an MCP server from an rmcp service:

```rust
use agent_client_protocol_core::mcp_server::McpServer;
use agent_client_protocol_rmcp::McpServerExt;

let server = McpServer::from_rmcp("my-server", MyRmcpService::new);

// Use as a handler in a proxy
Proxy.builder()
    .with_mcp_server(server)
    .connect_to(transport)
    .await?;
```

## Why a Separate Crate?

This crate is separate from `agent-client-protocol-core` to avoid coupling the core protocol crate to the `rmcp` dependency. This allows:

- `agent-client-protocol-core` to remain focused on the ACP protocol
- `agent-client-protocol-rmcp` to track `rmcp` updates independently
- Breaking changes in `rmcp` only require updating this crate

## Related Crates

- **[agent-client-protocol-core](../agent-client-protocol-core/)** — Core ACP protocol types and traits
- **[agent-client-protocol-tokio](../agent-client-protocol-tokio/)** — Tokio utilities for spawning agent processes

## License

Apache-2.0
