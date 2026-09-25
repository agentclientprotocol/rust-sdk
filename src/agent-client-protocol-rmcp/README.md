# agent-client-protocol-rmcp

[rmcp](https://docs.rs/rmcp) integration for [Agent Client Protocol](https://agentclientprotocol.com/) MCP servers.

## Overview

This crate bridges [rmcp](https://docs.rs/rmcp)-based MCP server implementations with the
runtime-agnostic MCP server framework from `agent-client-protocol`. It lets you define MCP tools in
Rust, serve them directly, or attach them to an ACP proxy.

Attached servers are advertised with the opt-in native MCP-over-ACP transport:
`McpServer::Acp` plus request-scoped `mcp/message` operations targeting MCP
2026-07-28. There is no MCP initialization or connect/disconnect exchange. This
crate does not enable the core SDK's `unstable_mcp_over_acp` feature merely to
build or directly serve a server. Enable this crate's matching
`unstable_mcp_over_acp` feature when using `with_mcp_server`. Use
`agent-client-protocol-polyfill` when the final agent has a modern MCP HTTP
client but does not consume ACP-transport MCP servers natively.

For a direct ACP client/agent example using real rmcp tools, run:

```sh
cargo run -p agent-client-protocol-rmcp --example stateless_native_mcp \
  --features unstable_mcp_over_acp,unstable_protocol_v2
```

## Usage

Use the `McpServerExt` trait to build an MCP server with tools:

```rust
use agent_client_protocol::{ConnectTo, mcp_server::McpServer, role::mcp};
use agent_client_protocol_rmcp::McpServerExt;

async fn serve(
    client_transport: impl ConnectTo<mcp::Server>,
) -> agent_client_protocol::Result<()> {
    let server = McpServer::<mcp::Client>::builder("my-tools").build();
    server.connect_to(client_transport).await
}
```

Choosing `mcp::Client` as the counterpart makes this a standalone MCP server
that implements `ConnectTo<mcp::Client>`.

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
| 4.x (unreleased)           | 2.x                   | 3.x  |
| 3.x                        | 2.x                   | 2.x  |
| 2.x                        | 1.x                   | 2.x  |
| 1.x                        | 1.x                   | 1.x  |

See the [rmcp 4.x integration migration guide](https://agentclientprotocol.github.io/rust-sdk/migration-rmcp-v4.html)
for adapting services to rmcp 3.x. The minimum supported Rust version remains
1.88. The dependency upgrade does not itself change the unstable ACP transport
envelopes or require applications to use modern MCP instead of rmcp's default
protocol version.

## Related Crates

- **[agent-client-protocol](../agent-client-protocol/)** — Core ACP protocol types and traits
- **[agent-client-protocol-conductor](../agent-client-protocol-conductor/)** — Proxy-chain orchestration

## License

Apache-2.0
