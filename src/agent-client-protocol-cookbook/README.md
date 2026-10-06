# agent-client-protocol-cookbook

Rustdoc recipes for building [Agent Client Protocol](https://agentclientprotocol.com/)
clients, agents, and proxies with the Rust SDK.

The cookbook covers one-shot prompts, permissions, reusable components,
ordered application dispatch, MCP tool attachment, conductor proxy chains,
and draft-v2 session coordination. It is documentation, not a runtime library
that applications need to depend on.

- [Browse the recipes](https://docs.rs/agent-client-protocol-cookbook)
- [SDK concepts](https://docs.rs/agent-client-protocol/latest/agent_client_protocol/concepts/)
- [Cargo feature selection](https://agentclientprotocol.github.io/rust-sdk/features.html)
- [Core v3 migration](https://agentclientprotocol.github.io/rust-sdk/migration-v3.html)

Examples declare the core and rmcp features they use in the cookbook's dev
dependencies. Applications should select their own required features rather
than assume the cookbook's feature combination is the core default.

From the SDK repository, use `cargo doc -p agent-client-protocol-cookbook --open`
to browse locally, and `just test` to run the workspace suite and cookbook
doctests.

## License

Apache-2.0. The published package includes `LICENSE`.
