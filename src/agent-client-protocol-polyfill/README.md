# agent-client-protocol-polyfill

Compatibility proxies for the
[Agent Client Protocol Rust SDK](https://docs.rs/agent-client-protocol).

`mcp_over_acp::McpOverAcpPolyfill::http()` re-exports native MCP-over-ACP servers
through loopback HTTP for a final agent that supports **MCP 2026-07-28** over
HTTP but not the native ACP binding. Insert it immediately before that agent
in a conductor proxy chain. It does not provide a fallback to older MCP
revisions or add MCP support to an agent with neither transport.

```toml
[dependencies]
agent-client-protocol-polyfill = "3"
```

The polyfill supports stable ACP v1 by default and explicitly enables the
core's `unstable_mcp_over_acp` feature. For a draft-v2 chain, enable
`unstable_protocol_v2` on both the polyfill and conductor: the conductor's
feature does not enable the polyfill's, and a polyfill built without it
negotiates the chain down to v1. The polyfill adapts `session/fork` in every
build. The independent `unstable_session_fork` feature also validates fork
requests against the schema's fork type.

- [API reference](https://docs.rs/agent-client-protocol-polyfill)
- [Placement, lifecycle, and compatibility guide](https://agentclientprotocol.github.io/rust-sdk/mcp-bridge.html)
- [Conductor guide](https://agentclientprotocol.github.io/rust-sdk/conductor.html)
- [Native MCP migration](https://agentclientprotocol.github.io/rust-sdk/migration-stateless-mcp.html)

## License

Apache-2.0. The published package includes `LICENSE`.
