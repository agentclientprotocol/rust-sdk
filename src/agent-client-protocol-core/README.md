# sacp -- the Symposium Agent Client Protocol (ACP) SDK

**sacp** is a Rust SDK for building [Agent-Client Protocol (ACP)](https://agentclientprotocol.com/) applications.
ACP is a protocol for communication between AI agents and their clients (IDEs, CLIs, etc.),
enabling features like tool use, permission requests, and streaming responses.

## What can you build with sacp?

- **Clients** that talk to ACP agents (like building your own Claude Code interface)
- **Proxies** that add capabilities to existing agents (like adding custom tools via MCP)
- **Agents** that respond to prompts with AI-powered responses

## Quick Start: Connecting to an Agent

The most common use case is connecting to an existing ACP agent as a client:

```rust
use sacp::ClientToAgent;
use sacp::schema::{InitializeRequest, VERSION as PROTOCOL_VERSION};

Client.builder()
    .name("my-client")
    .run_until(transport, async |cx| {
        // Initialize the connection
        cx.send_request(InitializeRequest {
            protocol_version: PROTOCOL_VERSION,
            client_capabilities: Default::default(),
            client_info: Default::default(),
            meta: None,
        }).block_task().await?;

        // Create a session and send a prompt
        cx.build_session_cwd()?
            .block_task()
            .run_until(async |mut session| {
                session.send_prompt("What is 2 + 2?")?;
                let response = session.read_to_string().await?;
                println!("{}", response);
                Ok(())
            })
            .await
    })
    .await
```

## Learning More

See the [crate documentation](https://docs.rs/sacp) for:

- **[Cookbook](https://docs.rs/sacp/latest/sacp/cookbook/)** - Patterns for building clients, proxies, and agents
- **[Examples](https://github.com/agentclientprotocol/symposium-acp/tree/main/src/sacp/examples)** - Working code you can run

You may also enjoy looking at:

- **[`yolo_one_shot_client.rs`](examples/yolo_one_shot_client.rs)** - Complete client that spawns an agent and sends a prompt
- **[`sacp-conductor`](https://crates.io/crates/sacp-conductor)** - Orchestrates proxy chains with a final agent

## Related Crates

- **[sacp-tokio](../sacp-tokio/)** - Tokio utilities for spawning agent processes
- **[sacp-proxy](../sacp-proxy/)** - Framework for building proxy components
- **[sacp-conductor](../sacp-conductor/)** - Binary for running proxy chains

## License

MIT OR Apache-2.0
