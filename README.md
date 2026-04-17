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
- [`agent-client-protocol-tokio`](./src/agent-client-protocol-tokio/) – Tokio utilities for spawning agent processes and wiring stdio transports.
- [`agent-client-protocol-rmcp`](./src/agent-client-protocol-rmcp/) – Integration with the [`rmcp`](https://docs.rs/rmcp) MCP SDK.
- [`agent-client-protocol-derive`](./src/agent-client-protocol-derive/) – Derive macros used by the core crate.

**Proxy orchestration**

- [`agent-client-protocol-conductor`](./src/agent-client-protocol-conductor/) – Binary and library that manages chains of proxy components.
- [`agent-client-protocol-trace-viewer`](./src/agent-client-protocol-trace-viewer/) – Interactive sequence-diagram viewer for conductor trace files.

**Patterns, examples, and testing**

- [`agent-client-protocol-cookbook`](./src/agent-client-protocol-cookbook/) – Practical patterns for clients, agents, and proxies, rendered as rustdoc.
- [`agent-client-protocol-test`](./src/agent-client-protocol-test/) – Shared test utilities and fixtures.
- [`yopo`](./src/yopo/) – "You Only Prompt Once", an example client.

## Documentation

- **API reference** for individual crates is on [docs.rs/agent-client-protocol](https://docs.rs/agent-client-protocol).
- **Design and architecture documentation** lives in the mdbook at [agentclientprotocol.github.io/rust-sdk](https://agentclientprotocol.github.io/rust-sdk/). Source is in [`md/`](./md/).

## Integrations

- [Schema](./schema/schema.json)
- [Agents](https://agentclientprotocol.com/overview/agents)
- [Clients](https://agentclientprotocol.com/overview/clients)
- Official Libraries
  - **Kotlin**: [`acp-kotlin`](https://github.com/agentclientprotocol/kotlin-sdk) – supports JVM, other targets are in progress
  - **Rust**: [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol) - See [examples/agent.rs](https://github.com/agentclientprotocol/rust-sdk/blob/main/src/agent-client-protocol/examples/agent.rs) and [examples/client.rs](https://github.com/agentclientprotocol/rust-sdk/blob/main/src/agent-client-protocol/examples/client.rs)
  - **TypeScript**: [`@agentclientprotocol/sdk`](https://www.npmjs.com/package/@agentclientprotocol/sdk) - See [examples/](https://github.com/agentclientprotocol/typescript-sdk/tree/main/src/examples)
- [Community Libraries](https://agentclientprotocol.com/libraries/community)

### Pull Requests

Pull requests should intend to close [an existing issue](https://github.com/agentclientprotocol/rust-sdk/issues).

### Issues

- **Bug Reports**: If you notice a bug in the protocol, please file [an issue](https://github.com/agentclientprotocol/rust-sdk/issues/new?template=05_bug_report.yml) and we will be in touch.
- **Protocol Suggestions**: If you'd like to propose additions or changes to the protocol, please start a [discussion](https://github.com/agentclientprotocol/rust-sdk/discussions/categories/protocol-suggestions) first. We want to make sure proposed suggestions align well with the project. If accepted, we can have a conversation around the implementation of these changes. Once that is complete, we will create an issue for pull requests to target.

### License

By contributing, you agree that your contributions will be licensed under the Apache 2.0 License.
