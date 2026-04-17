# Core Library Design

This document describes the design of the `agent-client-protocol` crate and its companion crates (`agent-client-protocol-tokio`, `agent-client-protocol-rmcp`).

For API usage, see the [rustdoc](https://docs.rs/agent-client-protocol) and [cookbook](https://docs.rs/agent-client-protocol-cookbook).

## Crate Organization

### agent-client-protocol

The core SDK. Provides:

- **Role types** (`Client`, `Agent`, `Proxy`, `Conductor`) - the identities in ACP
- **Connection builders** (`builder()`, `connect_to()`, `connect_with()`)
- **Message handling** (`on_receive_request`, `on_receive_notification`, `on_receive_dispatch`)
- **Protocol types** (`agent_client_protocol::schema::*`) - all ACP message types
- **MCP server builder** - for adding tools to proxies

### agent-client-protocol-tokio

Tokio-specific utilities:

- **Process spawning** - spawn agent/proxy processes and connect via stdio
- **Transport helpers** - convert tokio streams to ACP transports

### agent-client-protocol-rmcp

Integration with the [rmcp](https://docs.rs/rmcp) crate:

- **`McpServer::from_rmcp()`** - wrap an rmcp server as an ACP MCP server

## Role System

The type system is built around **roles** - the logical identity of an endpoint.

```mermaid
graph LR
    Client -->|connects to| Agent
    Agent -->|connects to| Client
    Proxy -->|connects to| Conductor
    Conductor -->|connects to| Proxy
```

### Counterpart Relationship

Each role has exactly one **counterpart** - who it connects to:

| Role        | Counterpart |
| ----------- | ----------- |
| `Client`    | `Agent`     |
| `Agent`     | `Client`    |
| `Proxy`     | `Conductor` |
| `Conductor` | `Proxy`     |

This is encoded in the type system: `impl ConnectTo<Client> for MyAgent` means "MyAgent can connect to a client" (i.e., MyAgent plays the Agent role).

### Peer Relationship

Some roles can communicate with multiple **peers**. The `Proxy` role is the key example:

```mermaid
graph TB
    subgraph "Proxy's view"
        Proxy
        Client[Client peer]
        Agent[Agent peer]
        Conductor[Conductor counterpart]
    end

    Proxy -.->|"send_to(Client, ...)"| Client
    Proxy -.->|"send_to(Agent, ...)"| Agent
    Proxy -->|"connect_to(conductor)"| Conductor
```

- **Counterpart** (Conductor) - who the proxy connects to (transport layer)
- **Peers** (Client, Agent) - who the proxy exchanges logical messages with

## Message Flow

### Dispatch Loop

Each connection runs a dispatch loop that processes incoming messages:

```mermaid
sequenceDiagram
    participant Transport
    participant DispatchLoop
    participant Handlers
    participant UserCode

    Transport->>DispatchLoop: incoming JSON-RPC message
    DispatchLoop->>Handlers: try handlers in order

    alt Handler matches
        Handlers->>UserCode: invoke callback
        UserCode-->>Handlers: return result
    else No handler matches
        Handlers->>DispatchLoop: default handler
    end
```

### Handler Chain

Handlers are tried in registration order. The first matching handler wins:

```mermaid
graph TD
    Message[Incoming Message]
    H1[Handler 1: InitializeRequest]
    H2[Handler 2: PromptRequest]
    H3[Handler 3: catch-all]

    Message --> H1
    H1 -->|not InitializeRequest| H2
    H2 -->|not PromptRequest| H3
    H3 --> Done[Handle or error]

    H1 -->|matches| Process1[Process Initialize]
    H2 -->|matches| Process2[Process Prompt]
```

### Ordering Guarantees

The dispatch loop provides sequential processing:

1. Messages are processed one at a time
2. A handler runs to completion before the next message is processed
3. Spawned tasks (`connection.spawn()`) run concurrently with the dispatch loop

**Important**: Don't block the dispatch loop. Use `spawn()` for long-running work.

## Connection Lifecycle

```mermaid
stateDiagram-v2
    [*] --> Building: builder()
    Building --> Building: on_receive_*()
    Building --> Connected: connect_to(transport)
    Building --> Connected: connect_with(transport, closure)
    Connected --> Running: dispatch loop starts
    Running --> [*]: connection closes
```

### Two Connection Modes

**Reactive mode** (`connect_to`): The connection runs handlers until the transport closes. Used for agents and proxies.

**Active mode** (`connect_with`): Runs a closure with access to the connection, then closes. Used for clients that drive the interaction.

## Key Source Files

| File                                         | Purpose                               |
| -------------------------------------------- | ------------------------------------- |
| `src/agent-client-protocol/src/role.rs`      | Role trait and type definitions       |
| `src/agent-client-protocol/src/role/acp.rs`  | Client, Agent, Proxy, Conductor roles |
| `src/agent-client-protocol/src/component.rs` | ConnectTo and Builder traits          |
| `src/agent-client-protocol/src/handler.rs`   | Connection builder implementation     |
| `src/agent-client-protocol/src/typed.rs`     | Dispatch type and handler matching    |
| `src/agent-client-protocol/src/mcp_server/`  | MCP server builder                    |
| `src/agent-client-protocol/src/concepts/`    | Rustdoc concept explanations          |

## Design Decisions

### Why Roles Instead of Links?

Earlier versions used "link types" that encoded both sides (e.g., `ClientToAgent`). Roles are simpler:

- One concept instead of two (role vs link)
- Role types double as peer selectors (`send_to(Agent, ...)`)
- Clearer mental model: "I am X, connecting to Y"

### Why Witness Macros?

The `on_receive_request!()` macros work around Rust's lack of return-type notation. They capture the return type of closures at the call site, enabling type inference to work.

### Why Not Traits for Handlers?

Handler closures are more ergonomic than trait implementations for most use cases. The `HandleDispatchFrom` trait exists for advanced cases (reusable handler components).
