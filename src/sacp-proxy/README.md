# sacp-proxy

Framework for building ACP proxy components that extend agent functionality.

## What are proxies?

Proxies are modular components that sit between an editor and an agent, intercepting and transforming messages. They enable **composable agent architectures** where functionality can be added without modifying the base agent.

```
Editor → Proxy 1 → Proxy 2 → Agent
```

Use cases:
- Add MCP tools/resources/prompts to any agent
- Inject context or modify prompts before they reach the agent
- Filter or transform agent responses
- Add logging, metrics, or policy enforcement

## Quick Start

The simplest proxy just forwards messages unchanged:

```rust
use sacp::JrConnection;
use sacp_proxy::{AcpProxyExt, McpServiceRegistry};

JrConnection::new(stdout, stdin)
    .name("my-proxy")
    .provide_mcp(McpServiceRegistry::default())  // Provide MCP services
    .proxy()  // Enable proxy mode
    .serve()
    .await?;
```

To intercept messages, add handlers before calling `.proxy()`:

```rust
JrConnection::new(stdout, stdin)
    .name("my-proxy")
    .on_receive_request_from_successor(|req: PromptRequest, cx| async move {
        // Intercept prompts from the successor (agent or next proxy)
        println!("Agent received prompt: {:?}", req.prompt);
        
        // Forward unchanged, or modify before forwarding
        cx.send_to_predecessor(req)
    })
    .provide_mcp(McpServiceRegistry::default())
    .proxy()
    .serve()
    .await?;
```

## Key Concepts

**Predecessor vs Successor:**
- **Predecessor** - The component closer to the editor (could be editor or another proxy)
- **Successor** - The component farther from the editor (could be agent or another proxy)

Messages flow: `Predecessor → This Proxy → Successor`

**Extension Traits:**
- `AcpProxyExt` - Adds proxy-specific methods to `JrConnection`
- `JrCxExt` - Adds forwarding methods (`send_to_predecessor`, `send_to_successor`)

## Examples

See the `examples/` directory:
- **[`minimal.rs`](examples/minimal.rs)** - Simplest possible proxy
- **[`with_mcp_server.rs`](examples/with_mcp_server.rs)** - Proxy that adds MCP tools

## How Proxies Work

When you call `.proxy()`, the framework:
1. Handles capability negotiation with predecessor and successor
2. Sets up message routing between components
3. Wraps your handlers to intercept specific message types
4. Forwards everything else automatically

You only need to handle the messages you want to intercept or transform.

## Related Crates

- **[sacp](../sacp/)** - Core ACP SDK
- **[sacp-conductor](../sacp-conductor/)** - Binary for orchestrating proxy chains
- **[sacp-tokio](../sacp-tokio/)** - Tokio utilities for spawning agents

## License

MIT OR Apache-2.0
