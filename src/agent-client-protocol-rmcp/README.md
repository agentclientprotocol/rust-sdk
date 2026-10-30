# agent-client-protocol-rmcp

[rmcp](https://docs.rs/rmcp) integration for [Agent Client Protocol](https://agentclientprotocol.com/) MCP servers.

## Overview

This crate bridges [rmcp](https://docs.rs/rmcp)-based MCP server implementations with the
runtime-agnostic MCP server framework from `agent-client-protocol`. It lets you define MCP tools in
Rust, serve them directly, or attach them to an ACP proxy.

Each supported rmcp major version has its own module behind a matching Cargo
feature. No rmcp feature is enabled by default; without one, this crate is
empty. Enable the feature for the rmcp major version your application uses:

| Feature  | Module                                | rmcp |
| -------- | ------------------------------------- | ---- |
| `rmcp_3` | `agent_client_protocol_rmcp::rmcp_3`  | 3.x  |

```toml
[dependencies]
agent-client-protocol-rmcp = { version = "5", features = ["rmcp_3"] }
rmcp = "3"
```

Attached servers are advertised with the opt-in native MCP-over-ACP transport:
`McpServer::Acp` plus request-scoped `mcp/message` for MCP 2026-07-28, without
an initialization prerequisite or connect/disconnect exchange. This
crate does not enable the core SDK's `unstable_mcp_over_acp` feature merely to
build or directly serve a server. Enable this crate's matching
`unstable_mcp_over_acp` feature when using `with_mcp_server`. Use
`agent-client-protocol-polyfill` when the final agent accepts HTTP but not
ACP-transport MCP servers.

`unstable_protocol_v2` and `unstable_mcp_over_acp` are independent passthrough
features. They apply to every rmcp module. Enable both when attaching a native
MCP server to draft ACP v2.

Native attachments lazily initialize one shared rmcp application service.
Standalone connections still invoke the factory separately. Each native
request owns its metadata, notifications, cancellation, and supervised cleanup.
Run the direct ACP example with:

```sh
cargo run -p agent-client-protocol-rmcp --example stateless_native_mcp \
  --features rmcp_3,unstable_protocol_v2,unstable_mcp_over_acp
```

## Usage

Use the `McpServerExt` trait to build an MCP server with tools:

```rust
use agent_client_protocol::{ConnectTo, mcp_server::McpServer, role::mcp};
use agent_client_protocol_rmcp::rmcp_3::McpServerExt;

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
use agent_client_protocol_rmcp::rmcp_3::McpServerExt;

let server = McpServer::from_rmcp("my-server", MyRmcpService::new);

// Use as a handler in a proxy
Proxy.v1()
    .with_mcp_server(server)
    .connect_to(transport)
    .await?;
```

## Why a Separate Crate?

This crate is separate from `agent-client-protocol` to avoid coupling the core protocol crate to the `rmcp` dependency or its features. This allows:

- `agent-client-protocol` to remain focused on the ACP protocol, with no rmcp
  dependency and no rmcp features
- `agent-client-protocol-rmcp` to support several `rmcp` major versions side
  by side, one feature and module per major version
- Integrations to adopt a new `rmcp` major version without waiting for an ACP
  major release

## Versioning

Both `agent-client-protocol` and `rmcp` are public dependencies of this crate:
their types and traits appear in its public API. Each supported rmcp major
version therefore gets its own feature and module, such as `rmcp_3` for rmcp
3.x, so that the rmcp types in a module's API never change major version:

- No rmcp feature is enabled by default. Enable the one matching your `rmcp`
  dependency.
- The features are additive. Enabling one adds its module and changes nothing
  else, so an application can enable two while it migrates between rmcp major
  versions.
- Support for a new rmcp major version arrives as a new feature and module in
  a minor release of this crate.
- Dropping support for an rmcp major version removes its feature and module,
  which is a breaking change and only happens in an ACP major release.

A source-incompatible major release of `agent-client-protocol` still requires
a major release of this crate; it shares its version with the other ACP crates.

| agent-client-protocol-rmcp | agent-client-protocol | rmcp                     |
| -------------------------- | --------------------- | ------------------------ |
| 5.x (unreleased)           | 5.x                   | 3.x (`rmcp_3` feature)   |
| 4.x                        | 3.x                   | 3.x                      |
| 3.x                        | 2.x                   | 2.x                      |
| 2.x                        | 1.x                   | 2.x                      |
| 1.x                        | 1.x                   | 1.x                      |

See the [v4 migration guide](https://agentclientprotocol.github.io/rust-sdk/migration-v4.html#the-rmcp-integration-moves-behind-the-rmcp_3-feature)
for moving to the `rmcp_3` feature and module, and the
[rmcp 4.x integration migration guide](https://agentclientprotocol.github.io/rust-sdk/migration-rmcp-v4.html)
for migrating both public dependencies in the combined core 3.x / integration
4.x / rmcp 3.x release. The minimum supported Rust version remains 1.88.
Native attachments also move to the
[request-scoped MCP binding](https://agentclientprotocol.github.io/rust-sdk/migration-stateless-mcp.html)
for MCP 2026-07-28 only; the old connect/disconnect binding is removed.
Standalone rmcp serving retains protocol negotiation. Custom transports and
low-level callers should follow the
[connection-driver migration](https://agentclientprotocol.github.io/rust-sdk/migration-connection-drivers.html),
and raw response adapters should follow the
[core 3.x migration guide](https://agentclientprotocol.github.io/rust-sdk/migration-v3.html).

## Related Crates

- **[agent-client-protocol](../agent-client-protocol/)** — Core ACP protocol types and traits
- **[agent-client-protocol-conductor](../agent-client-protocol-conductor/)** — Proxy-chain orchestration

## License

Apache-2.0
