# agent-client-protocol-core

Core protocol types and traits for the [Agent Client Protocol (ACP)](https://agentclientprotocol.com/).

ACP is a protocol for communication between AI agents and their clients (IDEs, CLIs, etc.),
enabling features like tool use, permission requests, and streaming responses.

## What can you build with this crate?

- **Clients** that talk to ACP agents (like building your own Claude Code interface)
- **Proxies** that add capabilities to existing agents (like adding custom tools via MCP)
- **Agents** that respond to prompts with AI-powered responses

## Quick Start: Connecting to an Agent

The most common use case is connecting to an existing ACP agent as a client:

```rust
use agent_client_protocol_core::{Client, Agent, ConnectTo};
use agent_client_protocol_core::schema::{InitializeRequest, ProtocolVersion};

Client.builder()
    .name("my-client")
    .connect_with(transport, async |cx| {
        // Initialize the connection
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;

        Ok(())
    })
    .await?;
```

## Learning More

See the [crate documentation](https://docs.rs/agent-client-protocol-core) for:

- **[Cookbook](https://docs.rs/agent-client-protocol-core/latest/agent_client_protocol_core/cookbook/)** — Patterns for building clients, proxies, and agents
- **[Examples](https://github.com/agentclientprotocol/rust-sdk/tree/main/src/agent-client-protocol-core/examples)** — Working code you can run

## Related Crates

- **[agent-client-protocol-tokio](../agent-client-protocol-tokio/)** — Tokio utilities for spawning agent processes
- **[agent-client-protocol-derive](../agent-client-protocol-derive/)** — Derive macros for JSON-RPC traits
- **[agent-client-protocol-trace-viewer](../agent-client-protocol-trace-viewer/)** — Interactive trace visualization

## License

Apache-2.0
