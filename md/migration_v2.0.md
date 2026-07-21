# Migrating from `agent-client-protocol` 1.x to 2.0

Version 2.0 changes the low-level in-process transport boundary so JSON-RPC frames remain intact
across components and adapters. It also gives `AcpAgent` an SDK-owned process-launch
configuration instead of reusing an MCP wire-schema type.

## Raw channels carry frames

`Channel` is now the single batch-aware in-process transport boundary. Its `rx` and `tx` carry
`TransportFrame`, not `Result<RawJsonRpcMessage, Error>`:

```rust
use agent_client_protocol::{Channel, RawJsonRpcMessage, TransportFrame};

# fn send(channel: &Channel, message: RawJsonRpcMessage) {
channel.tx.unbounded_send(TransportFrame::Single(message)).unwrap();
# }
```

`TransportFrame` distinguishes a valid single message, a non-empty `TransportBatch`, and an
explicit malformed wire value. Transport I/O failures are returned by the future from
`ConnectTo::into_channel_and_future`; they are never channel items. Components should preserve a
received frame intact when relaying it so batch response grouping remains correct.

The separate, hidden `FramedChannel` and `into_framed_channel_and_future` compatibility path no
longer exist. Implement only `ConnectTo::connect_to`, optionally overriding
`into_channel_and_future` for a direct channel adapter.

## `AcpAgent` has its own process configuration

`AcpAgent` now accepts `AcpAgentConfig` instead of the ACP wire-schema `McpServer`. An ACP agent
subprocess is not an MCP server, and its local launch settings should not depend on the v1
protocol schema:

```rust
use agent_client_protocol::{AcpAgent, AcpAgentConfig};

let agent = AcpAgent::new(
    AcpAgentConfig::new("my-agent")
        .arg("--verbose")
        .env("RUST_LOG", "info"),
);
let configuration = agent.config();
# let _ = configuration;
```

`server()` and `into_server()` are now `config()` and `into_config()`. The MCP-only `name` and
`_meta` fields have no equivalents because they never affected process launching. Use `command()`,
`arguments()`, and `environment()` to inspect the configuration.

The JSON configuration shape also changes. The MCP `type`, `name`, and `_meta` fields are removed,
and environment variables change from schema objects:

```json
{
  "type": "stdio",
  "name": "my-agent",
  "command": "python",
  "args": ["agent.py"],
  "env": [{ "name": "RUST_LOG", "value": "info" }]
}
```

to a string map:

```json
{
  "command": "python",
  "args": ["agent.py"],
  "env": { "RUST_LOG": "info" }
}
```

Command-string and `from_args` construction are unchanged. HTTP and SSE `McpServer` variants were
never valid subprocess launch configurations; callers using them must select an appropriate
network transport separately.

The deprecated `AcpAgent::zed_claude_code` and `AcpAgent::zed_codex` constructors were removed;
use `claude_agent` and `codex`. The `google_gemini` convenience constructor was also removed;
replace it with the explicit command:

```rust
let agent = AcpAgent::from_args([
    "npx",
    "-y",
    "--",
    "@google/gemini-cli@latest",
    "--experimental-acp",
]).unwrap();
```
