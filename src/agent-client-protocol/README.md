<a href="https://agentclientprotocol.com/" >
  <img alt="Agent Client Protocol" src="https://zed.dev/img/acp/banner-dark.webp">
</a>

# agent-client-protocol

Core protocol types and traits for the [Agent Client Protocol (ACP)](https://agentclientprotocol.com/).

ACP is a protocol for communication between AI agents and their clients (IDEs, CLIs, etc.),
enabling features like tool use, permission requests, and streaming responses.

## What can you build with this crate?

- **Clients** that talk to ACP agents (like building your own Claude Code interface)
- **Proxies** that add capabilities to existing agents (like adding custom tools via MCP)
- **Agents** that respond to prompts with AI-powered responses

## Quick Start: Connecting to an Agent

The most common use case is connecting to an existing ACP agent as a client.
This quick start uses stable protocol v1:

```rust,no_run
use agent_client_protocol::{AcpAgent, Client, Result};
use agent_client_protocol::schema::{ProtocolVersion, v1::InitializeRequest};

# async fn connect() -> Result<()> {
let agent = AcpAgent::from_args(["my-agent"])?;
Client.builder()
    .name("my-client")
    .connect_with(agent, async |cx| {
        // Initialize the connection
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;

        Ok(())
    })
    .await
# }
```

Draft protocol v2 is opt-in through `unstable_protocol_v2`. `Client.v2()`,
`Agent.v2()`, and `Proxy.v2()` callbacks receive a version-typed
`V2ConnectionTo` with
high-level, command-only session helpers because prompt acceptance and inbound
traffic are independent. Session updates and interactive requests use typed
connection handlers. See [Protocol V2](https://agentclientprotocol.github.io/rust-sdk/protocol-v2.html#high-level-v2-sessions).
`Proxy.builder()` remains the stable v1 entry point. Use
`Proxy.protocol_router()` to expose separate strict v1 and v2 proxy
implementations as one component; custom raw routing infrastructure can use
`Proxy.builder().without_acp_version_guard()`.

## MCP Server Attachment

The runtime-agnostic `mcp_server` module can build and directly serve standalone
MCP servers without enabling an ACP schema extension. Attaching one to ACP with
the `with_mcp_server` builder methods requires `unstable_mcp_over_acp`.
Attached servers are advertised with native `McpServer::Acp` declarations and
communicate through `mcp/connect`, `mcp/message`, and `mcp/disconnect`. Use
`agent-client-protocol-polyfill` immediately before an HTTP-capable agent.
Stable protocol v1 supports per-session and global proxy attachment. Draft
protocol v2 supports both scopes when both unstable features are enabled:
`Proxy.v2().with_mcp_server(...)` injects a global server into supported setup
requests, and `V2SessionBuilder::with_mcp_server(...)` attaches one to a single
`session/new`. Successful attachments remain active for the connection
lifetime. A v2 proxy can forward setup with
`V2SessionBuilder::on_proxy_session_start`; updates and interactive requests
remain independent connection traffic.

## Learning More

See the [crate documentation](https://docs.rs/agent-client-protocol) for:

- **[Cookbook](https://docs.rs/agent-client-protocol-cookbook)** — Patterns for building clients, proxies, and agents
- **[Examples](https://github.com/agentclientprotocol/rust-sdk/tree/main/src/agent-client-protocol/examples)** — Working code you can run

## Related Crates

- **[agent-client-protocol-http](../agent-client-protocol-http/)** — HTTP/SSE and WebSocket transports
- **[agent-client-protocol-rmcp](../agent-client-protocol-rmcp/)** — MCP tool builders and `rmcp` integration
- **[agent-client-protocol-derive](../agent-client-protocol-derive/)** — Derive macros for JSON-RPC traits
- **[agent-client-protocol-conductor](../agent-client-protocol-conductor/)** — Proxy-chain orchestration
- **[agent-client-protocol-polyfill](../agent-client-protocol-polyfill/)** — Compatibility proxies, including adapting MCP-over-ACP to HTTP
- **[agent-client-protocol-trace-viewer](../agent-client-protocol-trace-viewer/)** — Interactive trace visualization

## Contribution Policy

This project does not require a Contributor License Agreement (CLA). Instead, contributions are accepted under the following terms:

> By contributing to this project, you agree that your contributions will be licensed under the [Apache License, Version 2.0](https://www.apache.org/licenses/LICENSE-2.0). You affirm that you have the legal right to submit your work, that you are not including code you do not have rights to, and that you understand contributions are made without requiring a Contributor License Agreement (CLA).
