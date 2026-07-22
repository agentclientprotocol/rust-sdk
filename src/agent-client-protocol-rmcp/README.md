# agent-client-protocol-rmcp

[rmcp](https://docs.rs/rmcp) integration for [Agent Client Protocol](https://agentclientprotocol.com/) MCP servers.

## Overview

This crate bridges [rmcp](https://docs.rs/rmcp)-based MCP server implementations with the
runtime-agnostic MCP server framework from `agent-client-protocol`. It lets you define MCP tools in
Rust, serve them directly, or attach them to an ACP proxy.

Attached servers are advertised with the opt-in native MCP-over-ACP transport:
`McpServer::Acp` plus `mcp/connect`, `mcp/message`, and `mcp/disconnect`. This
crate does not enable the core SDK's `unstable_mcp_over_acp` feature merely to
build or directly serve a server. Enable this crate's matching
`unstable_mcp_over_acp` feature when using `with_mcp_server`. Use
`agent-client-protocol-polyfill` when the final agent accepts HTTP but not
ACP-transport MCP servers.

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
- Integrations to choose compatible `agent-client-protocol` and `rmcp` major
  versions explicitly

## Versioning

Both `agent-client-protocol` and `rmcp` are public dependencies of this crate:
their types and traits appear in its public API. A source-incompatible major
release of either dependency therefore requires a major release of this crate.

| agent-client-protocol-rmcp | agent-client-protocol | rmcp |
| -------------------------- | --------------------- | ---- |
| 3.x                        | 2.x                   | 2.x  |
| 2.x                        | 1.x                   | 2.x  |
| 1.x                        | 1.x                   | 1.x  |

## Related Crates

- **[agent-client-protocol](../agent-client-protocol/)** — Core ACP protocol types and traits
- **[agent-client-protocol-conductor](../agent-client-protocol-conductor/)** — Proxy-chain orchestration

## License

Apache-2.0
