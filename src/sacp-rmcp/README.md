# sacp-rmcp

rmcp integration for SACP proxy components.

## Overview

This crate provides integration between [rmcp](https://docs.rs/rmcp)-based MCP servers and the SACP proxy framework. It allows you to easily add rmcp services to your SACP proxies.

## Usage

Add `sacp-rmcp` to your `Cargo.toml`:

```toml
[dependencies]
sacp-proxy = "1.0"
sacp-rmcp = "0.1"
rmcp = "0.8"
```

Then use the extension trait to add rmcp servers to your registry:

```rust
use sacp_proxy::McpServiceRegistry;
use sacp_rmcp::McpServiceRegistryRmcpExt;

let registry = McpServiceRegistry::new();

// Add an rmcp-based MCP server
registry.add_rmcp_server("my-server", || MyRmcpService::new())?;

// Or chain multiple servers
let registry = McpServiceRegistry::new()
    .with_rmcp_server("server1", || Service1::new())?
    .with_rmcp_server("server2", || Service2::new())?;
```

## Why a Separate Crate?

The `sacp-rmcp` crate is separate from `sacp-proxy` to avoid tying the stable 1.0 `sacp-proxy` API to the 0.x `rmcp` crate. This allows:

- `sacp-proxy` to maintain a stable 1.0 API
- `sacp-rmcp` to track `rmcp` updates independently
- Breaking changes in `rmcp` only require updating `sacp-rmcp`, not `sacp-proxy`

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](../../LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../../LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
