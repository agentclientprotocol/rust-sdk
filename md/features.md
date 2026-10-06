# Cargo Features

Starting with the upcoming **3.x** release, the core `agent-client-protocol`
crate enables **no features by default**. Protocol serialization, connections,
sessions, custom MCP servers, and the `Channel`, `Lines`, and `ByteStreams`
adapters remain available without native I/O or JSON Schema generation.

```toml
[dependencies]
agent-client-protocol = "3"
```

You do not need `default-features = false` for a lean core dependency.

## Native transports

| Feature | Provides | Native dependencies |
| --- | --- | --- |
| `process` | `AcpAgent` and `AcpAgentConfig` for launching agents | `async-process`, `async-io`, `shell-words`, platform process support |
| `stdio` | `Stdio` for serving an agent or proxy over standard input/output | `blocking` |

These features are independent. `process` handles the child process's stdio
itself and does not enable `stdio`; `stdio` does not enable process spawning.
`LineDirection`, used by native debug callbacks, is exported with either.

For a client that launches an external agent:

```toml
[dependencies]
agent-client-protocol = { version = "3", features = ["process"] }
```

For an agent that uses `Stdio::new()`:

```toml
[dependencies]
agent-client-protocol = { version = "3", features = ["stdio"] }
```

The conductor explicitly enables both native features. Other libraries using
only generic transports need neither. The native dependencies and exports
remain excluded on WebAssembly, even when these features are enabled.

## JSON Schema generation and typed tools

Opt into `schemars` to generate JSON Schemas for protocol types and typed tools:

```toml
[dependencies]
agent-client-protocol = { version = "3", features = ["schemars"] }
```

This enables the SDK's direct dependency and the matching
`agent-client-protocol-schema` feature. Without it, protocol types still support
serialization and deserialization. Within `mcp_server`, these typed APIs require
`schemars`:

- `McpTool`
- `McpToolRegistry`, `RegisteredMcpTool`, and `EnabledTools`
- `McpToolMetadata` and `McpToolSchema`
- The `tool_fn` and `tool_fn_mut` functions

`McpServer`, `McpServerConnect`, `McpConnectionTo`, and `McpConnectionContext`
remain available without it. MCP-over-ACP attachment still only requires its
protocol feature, not JSON Schema generation.

The `agent-client-protocol-rmcp` integration explicitly enables `schemars` for
its tool builders. HTTP and polyfill transports do not enable it themselves.
Cargo features are additive: another dependency that requests a feature can
enable it for the same core crate, even if your direct dependency omits it.

## Transport and tooling dependencies

Internal crates request only the core capabilities they use:

| Consumer | Core features requested |
| --- | --- |
| HTTP client/server | None; transport sides remain opt-in through `client` and `server` |
| MCP-over-ACP polyfill | `unstable_mcp_over_acp`, with independent draft-v2 and session-fork passthrough features |
| rmcp integration | `schemars`, with independent draft-v2 and MCP-over-ACP passthrough features |
| Conductor | `process`, `stdio`, and `unstable_mcp_over_acp` for launching components, serving stdio, and classifying MCP traces |
| YOPO | `process` for launching the agent |
| Test utilities | `process` and `stdio` for fixture binaries and launch helpers |

Conductor integration tests and cookbook recipes explicitly enable the rmcp
integration's `unstable_mcp_over_acp` feature when attaching native servers.
Conductor tests request `schemars` separately from production dependencies, and
enable draft-v2 polyfill support only with the conductor's `unstable_protocol_v2`
feature. Internal consumers disable test-utility defaults rather than enabling
the core's entire `unstable` aggregate incidentally.

The Axum consumers share HTTP1/Tokio serving and tracing/tower-log observability,
but do not enable its default extractor features. The HTTP server opts into
WebSockets, the polyfill opts into JSON responses, and HTTP tests opt into JSON
extraction. The trace viewer serializes its responses directly and requires
neither. Axum macros are not needed. Tracing subscribers retain environment
filtering and text output without JSON log formatting.

The core SDK enables the futures executor only for tests and doctests. The
conductor's Tokio/futures compatibility adapter is test-only, and the test
utilities need it only for the `arrow_proxy` example. The rmcp integration keeps
compatibility adapters and cancellation tokens in production; rmcp itself still
enables the futures executor transitively.

## Draft protocol features

Unstable protocol features are independent of the native and schema features.
For example, to use draft v2 and MCP-over-ACP without JSON Schema generation:

```toml
[dependencies]
agent-client-protocol = { version = "3", features = ["unstable_protocol_v2", "unstable_mcp_over_acp"] }
```

The `unstable` aggregate does not select draft v2; enable `unstable_protocol_v2`
explicitly when using that version.

The `unstable_subagents` feature exposes the schema's subagent capabilities,
updates, and session-message types. It is also included in `unstable`. The
existing `session/update` notification route carries subagent updates in v1 and,
with `unstable_protocol_v2`, draft v2. This is schema support, not a subagent
scheduler or an implementation of application-level ownership and lifecycle
policy.

## WebAssembly and the rmcp integration

The core library compiles for `wasm32-wasip1` and `wasm32-wasip2` with default
features. No WASI feature or default-feature opt-out is required. For
JavaScript-hosted `wasm32-unknown-unknown`, enable `wasm_js`; it selects Web
Crypto through `wasm-bindgen` for UUID randomness. The target does not imply a
JavaScript host, so this feature remains opt-in.

The rmcp integration library also compiles for both WASI targets, including
native MCP-over-ACP attachment support. Its production dependencies use Tokio's
single-thread-capable runtime and async I/O utilities, not Tokio stdio or the
multithreaded runtime. Native examples and tests enable those conveniences
separately. An application using rmcp macros or stdio transports opts into the
corresponding features on its own `rmcp` dependency.

Neither library provides a WASI executor or host I/O adapter. Supply a compatible
runtime and transport; see [Transport Architecture](./transport-architecture.md).
This portability does not extend to the native HTTP server, polyfill, or
conductor crates.

## Migrating from 2.x defaults

A dependency that previously relied on native transports and typed tools:

```toml
# Before
agent-client-protocol = "2.2"

# After: select only the capabilities your application actually uses.
agent-client-protocol = { version = "3", features = ["process", "stdio", "schemars"] }
```

A protocol-only application can instead use `agent-client-protocol = "3"`.
Applications that already disabled defaults must still enable `process` or
`stdio` if they use those adapters: native I/O used to be unconditional on
native targets, independently of defaults.

Core examples declare their required features explicitly. For the v2 example
pair, build with `--features process,stdio,unstable_protocol_v2`. The v1
`simple_agent` requires `stdio`, while `yolo_one_shot_client` requires `process`.
