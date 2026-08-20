<a href="https://agentclientprotocol.com/" >
  <img alt="Agent Client Protocol" src="https://zed.dev/img/acp/banner-dark.webp">
</a>

# Agent Client Protocol

The Agent Client Protocol (ACP) standardizes communication between _code editors_ (interactive programs for viewing and editing source code) and _coding agents_ (programs that use generative AI to autonomously modify code).

Learn more at [agentclientprotocol.com](https://agentclientprotocol.com/).

This repository is the official **Rust SDK** for ACP. It provides crates for building clients, agents, and proxies, plus a conductor that orchestrates chains of proxies between an editor and an agent so that behavior can be extended without modifying the agent itself.

## Crates

**Core SDK**

- [`agent-client-protocol`](./src/agent-client-protocol/) – Roles (Client, Agent, Proxy, Conductor), connection builders, handlers, and protocol types.
- [`agent-client-protocol-http`](./src/agent-client-protocol-http/) – HTTP/SSE and WebSocket transports.
- [`agent-client-protocol-rmcp`](./src/agent-client-protocol-rmcp/) – Integration with the [`rmcp`](https://docs.rs/rmcp) MCP SDK.
- [`agent-client-protocol-derive`](./src/agent-client-protocol-derive/) – Derive macros used by the core crate.

Native MCP-over-ACP support is currently opt-in through the core crate's
`unstable_mcp_over_acp` feature. Standalone MCP servers need no ACP transport
feature; the rmcp integration exposes a matching passthrough feature when those
servers are attached to ACP. Stable protocol v1 supports per-session and global
proxy attachment. Draft protocol v2 supports the same two attachment scopes
when both `unstable_protocol_v2` and `unstable_mcp_over_acp` are enabled:
`Proxy.v2().with_mcp_server(...)` injects a global declaration into each
supported session setup request, while
`V2SessionBuilder::with_mcp_server(...)` attaches a server to one new session
and `V2ResumeSessionBuilder::with_mcp_server(...)` attaches one while resuming.
With `unstable_session_fork`, `V2ForkSessionBuilder::with_mcp_server(...)`
attaches one while forking. Successful v2 attachments remain active for the
connection lifetime, and all three builders expose `on_proxy_session_start` to
forward proxied setup without coupling later session events to that response.

**Proxy orchestration**

- [`agent-client-protocol-conductor`](./src/agent-client-protocol-conductor/) – Binary and library that manages chains of proxy components.
- [`agent-client-protocol-polyfill`](./src/agent-client-protocol-polyfill/) – Compatibility proxies, including adapting native MCP-over-ACP declarations to HTTP for agents that cannot consume them directly.
- [`agent-client-protocol-trace-viewer`](./src/agent-client-protocol-trace-viewer/) – Interactive sequence-diagram viewer for conductor trace files.

**Patterns, examples, and testing**

- [`agent-client-protocol-cookbook`](./src/agent-client-protocol-cookbook/) – Practical patterns for clients, agents, and proxies, rendered as rustdoc.
- [`agent-client-protocol-test`](./src/agent-client-protocol-test/) – Shared test utilities and fixtures.
- [`yopo`](./src/yopo/) – "You Only Prompt Once", an example client.

## Documentation

- **API reference** for individual crates is on [docs.rs/agent-client-protocol](https://docs.rs/agent-client-protocol).
- **Design and architecture documentation** lives in the mdbook at [agentclientprotocol.github.io/rust-sdk](https://agentclientprotocol.github.io/rust-sdk/). Source is in [`md/`](./md/).
- **Draft protocol v2** setup, version-typed connections, and high-level
  session usage are covered in [Protocol V2](./md/protocol-v2.md). To run a
  complete v2 agent/client pair first, use the
  [Runnable V2 Quickstart](./md/protocol-v2-quickstart.md).

`Client.builder()`, `Agent.builder()`, and `Proxy.builder()` remain stable-v1
entry points; their `.v2()` counterparts select the draft-v2 API. With
`unstable_protocol_v2`, use `Proxy.protocol_router()` to expose separate strict
v1 and v2 proxy implementations as one component. Custom raw routing
infrastructure can use `without_acp_version_guard`.

## Integrations

- [Protocol schema and documentation](https://agentclientprotocol.com/)
- [Agents](https://agentclientprotocol.com/overview/agents)
- [Clients](https://agentclientprotocol.com/overview/clients)
- Official Libraries
  - **Kotlin**: [`acp-kotlin`](https://github.com/agentclientprotocol/kotlin-sdk) – supports JVM, other targets are in progress
  - **Rust**: [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol) - See the stable-v1 [agent](./src/agent-client-protocol/examples/simple_agent.rs) and [client](./src/agent-client-protocol/examples/yolo_one_shot_client.rs), or the draft-v2 [agent](./src/agent-client-protocol/examples/simple_agent_v2.rs) and [client](./src/agent-client-protocol/examples/v2_one_shot_client.rs) examples
  - **TypeScript**: [`@agentclientprotocol/sdk`](https://www.npmjs.com/package/@agentclientprotocol/sdk) - See [examples/](https://github.com/agentclientprotocol/typescript-sdk/tree/main/src/examples)
- [Community Libraries](https://agentclientprotocol.com/libraries/community)

### Pull Requests

Pull requests should intend to close [an existing issue](https://github.com/agentclientprotocol/rust-sdk/issues).

### Issues

- **Bug Reports**: If you notice a bug in the protocol, please file [an issue](https://github.com/agentclientprotocol/rust-sdk/issues/new?template=05_bug_report.yml) and we will be in touch.
- **Protocol Suggestions**: If you'd like to propose additions or changes to the protocol, please start a [discussion](https://github.com/agentclientprotocol/rust-sdk/discussions/categories/protocol-suggestions) first. We want to make sure proposed suggestions align well with the project. If accepted, we can have a conversation around the implementation of these changes. Once that is complete, we will create an issue for pull requests to target.

### License

By contributing, you agree that your contributions will be licensed under the Apache 2.0 License.
