# sacp-tokio

Tokio-specific utilities for working with ACP agents.

## What's in this crate?

This crate provides helpers for spawning and connecting to ACP agents using the Tokio async runtime:

- **`AcpAgent`** - Configuration for spawning agent processes
- **`JrConnectionExt`** - Extension trait that adds `JrConnection::to_agent()` for easy agent spawning

## Usage

The main use case is spawning an agent process and creating a connection to it in one step:

```rust
use sacp::JrConnection;
use sacp_tokio::{AcpAgent, JrConnectionExt};

let agent = AcpAgent::from_str("python my_agent.py")?;

JrConnection::to_agent(agent)?
    .on_receive_notification(|notif: SessionNotification, _cx| async move {
        println!("Agent update: {:?}", notif);
        Ok(())
    })
    .with_client(|cx| async move {
        // Initialize and interact with the agent
        let response = cx.send_request(InitializeRequest { ... })
            .block_task()
            .await?;
        Ok(())
    })
    .await?;
```

The agent process is managed automatically - it's spawned when you call `to_agent()`, 
and killed when the connection is dropped.

## When to use this crate

Use `sacp-tokio` when you need to:
- Spawn agent processes from your code
- Test agents by programmatically launching them
- Build tools that orchestrate multiple agents

If you're implementing an agent that listens on stdin/stdout, you only need the core `sacp` crate.

## Related Crates

- **[sacp](../sacp/)** - Core ACP SDK (use this for building agents)
- **[sacp-proxy](../sacp-proxy/)** - Framework for building ACP proxies
- **[sacp-conductor](../sacp-conductor/)** - Binary for orchestrating proxy chains

## License

MIT OR Apache-2.0
