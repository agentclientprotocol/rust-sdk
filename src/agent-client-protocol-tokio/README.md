# agent-client-protocol-tokio

Tokio-based utilities for working with [ACP](https://agentclientprotocol.com/) agents.

## What's in this crate?

This crate provides helpers for spawning and connecting to ACP agents using the Tokio async runtime:

- **`AcpAgent`** — Configuration for spawning agent processes, parseable from command strings or JSON
- **`Stdio`** — A transport that connects over stdin/stdout with optional debug logging

## Usage

The main use case is spawning an agent process and connecting to it:

```rust
use agent_client_protocol_core::{Client, ConnectTo};
use agent_client_protocol_tokio::AcpAgent;
use std::str::FromStr;

let agent = AcpAgent::from_str("python my_agent.py")?;

// The agent process is spawned automatically when connected
Client.builder()
    .name("my-client")
    .connect_to(agent)
    .await?;
```

You can also add debug logging to inspect the wire protocol:

```rust
use agent_client_protocol_tokio::{AcpAgent, LineDirection};

let agent = AcpAgent::from_str("python my_agent.py")?
    .with_debug(|line, direction| {
        eprintln!("{direction:?}: {line}");
    });
```

## When to use this crate

Use `agent-client-protocol-tokio` when you need to:

- Spawn agent processes from your code
- Test agents by programmatically launching them
- Build tools that orchestrate multiple agents

If you're implementing an agent that listens on stdin/stdout, you only need the core
[`agent-client-protocol-core`](../agent-client-protocol-core/) crate.

## Related Crates

- **[agent-client-protocol-core](../agent-client-protocol-core/)** — Core ACP protocol types and traits
- **[agent-client-protocol-derive](../agent-client-protocol-derive/)** — Derive macros for JSON-RPC traits
- **[agent-client-protocol-trace-viewer](../agent-client-protocol-trace-viewer/)** — Interactive trace visualization

## License

Apache-2.0
