# sacp-tee

A debugging proxy that transparently logs all ACP traffic to a file.

## What's in this crate?

This crate provides a pass-through proxy that sits between two ACP endpoints, forwarding all messages while recording them to a log file for debugging purposes:

- **`Tee`** - A `Component` that can be inserted into any proxy chain to capture traffic
- **`TeeHandler`** - The message handler that logs requests, responses, and notifications
- **`LogWriter`** - An async actor that writes log entries to disk
- **`LogEntry`** - Structured log entries with timestamps and direction metadata

## Usage

### As a standalone binary

```bash
# Log all traffic between a client and an agent
sacp-tee --log-file debug.log
```

### As a component in a proxy chain

```rust
use sacp_tee::Tee;
use sacp::component::Component;
use std::path::PathBuf;

// Insert the tee into a proxy chain
Tee::new(PathBuf::from("debug.log"))
    .serve(downstream_component)
    .await?;
```

## Log format

Each line in the log file is a JSON object:

```json
{"timestamp":"2026-03-01T12:00:00Z","direction":"downstream","message":{"id":1,"method":"initialize","params":{...}}}
{"timestamp":"2026-03-01T12:00:01Z","direction":"upstream","message":{"id":1,"result":{...}}}
```

## When to use this crate

Use `sacp-tee` when you need to:
- Debug message flow between ACP clients and agents
- Capture traffic for analysis or replay
- Diagnose protocol-level issues in a proxy chain

## Related Crates

- **[sacp](../sacp/)** - Core ACP SDK
- **[sacp-proxy](../sacp-proxy/)** - Framework for building ACP proxies
- **[sacp-conductor](../sacp-conductor/)** - Binary for orchestrating proxy chains
- **[sacp-tokio](../sacp-tokio/)** - Tokio-specific utilities for spawning agents

## License

MIT OR Apache-2.0
