# Agent Client Protocol Rust SDK

This repository contains the Rust SDK for the [Agent-Client Protocol (ACP)](https://agentclientprotocol.com/).

## For Users

**If you want to build something with these crates**, see the rustdoc:

- **[`agent-client-protocol`](https://docs.rs/agent-client-protocol)** - Core SDK for building clients, agents, and proxies
- **[`agent-client-protocol-cookbook`](https://docs.rs/agent-client-protocol-cookbook)** - Practical patterns and examples
- **[`agent-client-protocol-conductor`](https://docs.rs/agent-client-protocol-conductor)** - Running proxy chains

The `agent-client-protocol` crate includes a [`concepts`](https://docs.rs/agent-client-protocol/latest/agent_client_protocol/concepts/) module that explains how connections, sessions, callbacks, and message ordering work.

## For Maintainers and Agents

**This book** documents the design and architecture for people working on the codebase itself.

### Repository Structure

```
src/
├── agent-client-protocol/              # Core protocol SDK
├── agent-client-protocol-tokio/        # Tokio utilities (process spawning)
├── agent-client-protocol-rmcp/         # Integration with rmcp crate
├── agent-client-protocol-cookbook/     # Usage patterns (rendered as rustdoc)
├── agent-client-protocol-derive/       # Proc macros
├── agent-client-protocol-conductor/    # Conductor binary and library
├── agent-client-protocol-test/         # Test utilities and fixtures
├── agent-client-protocol-trace-viewer/ # Trace visualization tool
└── yopo/                               # "You Only Prompt Once" example client
```

### Crate Relationships

```mermaid
graph TD
    acp[agent-client-protocol<br/>Core SDK]
    tokio[agent-client-protocol-tokio<br/>Process spawning]
    rmcp[agent-client-protocol-rmcp<br/>rmcp integration]
    conductor[agent-client-protocol-conductor<br/>Proxy orchestration]
    cookbook[agent-client-protocol-cookbook<br/>Usage patterns]

    tokio --> acp
    rmcp --> acp
    conductor --> acp
    conductor --> tokio
    cookbook --> acp
    cookbook --> rmcp
    cookbook --> conductor
```

### Key Design Documents

- [Core Library Design](./design.md) - How `agent-client-protocol`, `agent-client-protocol-tokio`, and `agent-client-protocol-rmcp` are organized
- [Conductor Design](./conductor.md) - How the conductor orchestrates proxy chains
- [Protocol Reference](./protocol.md) - Wire protocol details and extension methods
- [P/ACP Specification](./proxying-acp.md) - The full proxy protocol specification
- [Migrating to v0.11](./migration_v0.11.x.md) - Upgrade guide from 0.10.x to 0.11
