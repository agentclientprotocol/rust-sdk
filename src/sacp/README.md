# sacp -- the Symposium Agent Client Protocol (ACP) SDK

**sacp** is a Rust SDK for building agents and editors using the [Agent-Client Protocol (ACP)](https://agentclientprotocol.com/). It makes it easy to build ACP editors and clients -- or, indeed, any JSON-RPC-based application.

## Quick Start

Building an ACP agent is straightforward with sacp's type-safe API:

```rust
// Start by creating an agent talking on stdout/stdin
JrConnection::new(
    tokio::io::stdout().compat_write(),
    tokio::io::stdin().compat(),
)
.name("my-agent") // Give it a name for logging purposes
.on_receive_request(async move |initialize: InitializeRequest, request_cx| {
    // Create one or more request handlers -- these are attempted in order.
    // You can do anything you want in here, but you should eventually
    // respond to the request with `request_cx.respond(...)`:
    request_cx.respond(InitializeResponse {
        protocol_version: initialize.protocol_version,
        agent_capabilities: AgentCapabilities::default(),
        auth_methods: Default::default(),
        agent_info: Default::default(),
        meta: Default::default(),
    })
})
.on_receive_message(async move |message: MessageAndCx<UntypedMessage, UntypedMessage>| {
    // You can also handle any kind of message:
    message.respond_with_error(sacp::util::internal_error("TODO"))
})
.serve() // Finally, start the server (or use `with_client`)
.await
```

## Learning more

You can learn more in the [docs for `JrConnection`](https://docs.rs/symposium-acp/latest/symposium_acp/jr_connection/struct.JrConnection.html) or on our [Github Pages](https://github.com/symposium-acp/symposium-acp) site.

You may also enjoy looking at some of these examples:

- **[`simple_agent.rs`](examples/simple_agent.rs)** - Minimal agent implementation
- **[`yolo_one_shot_client.rs`](examples/yolo_one_shot_client.rs)** - Complete client that spawns an agent and sends a prompt
- **[`elizacp`](https://crates.io/crates/elizacp)** - Full working agent with session management (also useful for testing)
- **[`sacp-conductor`](https://crates.io/crates/sacp-conductor)** - The "conductor" is an ACP agent that composes [proxies](https://crates.io/crates/sacp-conductor) components with a final agent.

## Related Crates

- **[sacp-proxy](../sacp-proxy/)** - Framework for building ACP proxies that extend agent behavior
- **[sacp-tokio](../sacp-tokio/)** - Tokio-specific utilities (process spawning, connection management)
- **[sacp-conductor](../sacp-conductor/)** - Binary for orchestrating proxy chains

## License

MIT OR Apache-2.0
