# MCP Bridge: Proxying MCP over ACP

The **MCP Bridge** enables agents without native MCP-over-ACP support to work with proxy components that provide MCP servers using ACP transport (`acp:$UUID`).

## Problem Statement

Proxy components may want to expose MCP servers to agents using ACP as the transport layer. This allows:

- Dynamic MCP server registration during session creation
- Proxies to correlate MCP tool calls with specific ACP sessions
- Unified protocol handling (everything flows through ACP messages)

However, many agents only support traditional MCP transports (stdio, SSE). The conductor bridges this gap by:

1. Accepting `acp:$UUID` URLs in `session/new` requests
2. Transforming them into stdio-based MCP servers the agent can connect to
3. Routing MCP messages between the agent (stdio) and proxies (ACP `_mcp/*` messages)

## High-Level Architecture

```mermaid
flowchart LR
    Proxy[Proxy Component]
    Conductor[Conductor]
    Agent[Agent Process]
    Bridge[MCP Bridge Process]

    Proxy -->|session/new with acp: URL| Conductor
    Conductor -->|session/new with stdio| Agent
    Agent <-->|stdio| Bridge
    Bridge <-->|TCP| Conductor
    Conductor <-->|_mcp/* messages| Proxy
```

**Key decision**: Use stdio + TCP bridge instead of direct stdio to agent, because:

- Preserves agent isolation (agent only sees stdio)
- Enables connection multiplexing (multiple bridges to one conductor)
- Simplifies lifecycle management (bridge exits when agent closes stdio)

## Session Initialization Flow

The conductor transforms MCP servers during session creation and correlates them with session IDs.

```mermaid
sequenceDiagram
    participant Proxy
    participant Conductor
    participant Listener as MCP Bridge<br/>Listener Actor
    participant Agent

    Note over Proxy: Wants to provide MCP server<br/>with session-specific context

    Proxy->>Conductor: session/new {<br/>  mcp_servers: [{<br/>    name: "research-tools",<br/>    url: "acp:uuid-123"<br/>  }]<br/>}

    Note over Conductor: Detects acp: transport<br/>Agent lacks mcp_acp_transport capability

    Conductor->>Conductor: Bind TCP listener on port 54321

    Conductor->>Listener: Spawn MCP Bridge Listener<br/>(acp_url: "acp:uuid-123")

    Note over Listener: Listening on TCP port 54321<br/>Waiting for session_id

    Conductor->>Agent: session/new {<br/>  mcp_servers: [{<br/>    name: "research-tools",<br/>    command: "conductor",<br/>    args: ["mcp", "54321"]<br/>  }]<br/>}

    Agent-->>Conductor: session/new response {<br/>  session_id: "sess-abc"<br/>}

    Note over Conductor: Extract session_id from response

    Conductor->>Listener: Send session_id: "sess-abc"<br/>(via oneshot channel)

    Note over Listener: Now has session_id<br/>Ready to accept connections

    Conductor-->>Proxy: session/new response {<br/>  session_id: "sess-abc"<br/>}

    Note over Agent: Later: spawns MCP bridge process

    Agent->>Bridge: spawn process:<br/>conductor mcp 54321

    Bridge->>Listener: TCP connect to localhost:54321

    Note over Listener: Connection arrives<br/>Session ID already known

    Listener->>Conductor: McpConnectionReceived {<br/>  acp_url: "acp:uuid-123",<br/>  session_id: "sess-abc"<br/>}

    Conductor->>Proxy: _mcp/connect {<br/>  acp_url: "acp:uuid-123",<br/>  session_id: "sess-abc"<br/>}

    Proxy-->>Conductor: _mcp/connect response {<br/>  connection_id: "conn-xyz"<br/>}

    Note over Proxy: Can correlate connection<br/>with session context

    Conductor-->>Listener: connection_id: "conn-xyz"

    Note over Listener,Bridge: Bridge now active<br/>Routes MCP <-> ACP messages
```

### Key Decisions

**Why spawn TCP listener before getting session_id (during request, not response)?**

- Agent may spawn bridge process immediately after receiving `session/new` response
- If listener doesn't exist yet, bridge connection fails with "connection refused"
- Spawning during request ensures TCP port is ready before agent receives response
- Session_id delivered asynchronously via oneshot channel once response arrives

**Why send session_id to listener before forwarding response?**

- Ensures session_id is available before agent spawns bridge process
- Eliminates race condition where TCP connection arrives before session_id known
- Listener blocks on receiving session_id, guaranteeing it's available when needed

**Why include session_id in `_mcp/connect`?**

- Proxies need to correlate MCP connections with ACP sessions
- Example: Research proxy remembers session context (current task, preferences)
- Without session_id, proxy has no way to associate connection with session state

**Why use oneshot channel for session_id delivery?**

- Listener spawned during request handling (before response available)
- Response comes asynchronously from agent
- Oneshot channel delivers session_id exactly once when response arrives
- Clean separation: listener setup (during request) vs session_id delivery (during response)

## Connection Lifecycle

Once the MCP connection is established, the bridge routes messages bidirectionally:

```mermaid
sequenceDiagram
    participant Agent
    participant Bridge as MCP Bridge<br/>Process
    participant Listener as Bridge Listener<br/>Actor
    participant Conductor
    participant Proxy

    Note over Agent,Proxy: Connection established (connection_id: "conn-xyz")

    Agent->>Bridge: MCP tools/list request<br/>(stdio JSON-RPC)

    Bridge->>Listener: JSON-RPC over TCP

    Listener->>Conductor: Raw JSON-RPC message

    Conductor->>Proxy: _mcp/request {<br/>  connection_id: "conn-xyz",<br/>  method: "tools/list",<br/>  params: {...}<br/>}

    Note over Proxy: Has connection_id -> session_id mapping<br/>Can use session context

    Proxy-->>Conductor: _mcp/request response {<br/>  tools: [...]<br/>}

    Conductor-->>Listener: JSON-RPC response

    Listener-->>Bridge: JSON-RPC over TCP

    Bridge-->>Agent: MCP response<br/>(stdio JSON-RPC)

    Note over Agent: Agent disconnects

    Agent->>Bridge: Close stdio

    Bridge->>Listener: Close TCP connection

    Listener->>Conductor: McpConnectionDisconnected {<br/>  connection_id: "conn-xyz"<br/>}

    Conductor->>Proxy: _mcp/disconnect {<br/>  connection_id: "conn-xyz"<br/>}

    Note over Proxy: Clean up session state
```

### Key Decisions

**Why route through conductor instead of direct bridge-to-proxy?**

- Maintains consistent message ordering through central conductor queue
- Preserves conductor as sole message router
- Simplifies error handling and lifecycle management

**Why use connection_id instead of session_id in `_mcp/*` messages?**

- One session can have multiple MCP connections (multiple servers)
- Connection_id uniquely identifies the bridge instance
- Proxies maintain `connection_id -> session_id` mapping internally

**Why send disconnect notification?**

- Allows proxies to clean up session-specific state
- Enables resource cleanup (close files, release locks, etc.)
- Provides explicit lifecycle boundary

## Race Condition Handling

The session_id delivery mechanism prevents a race condition:

```mermaid
sequenceDiagram
    participant Conductor
    participant Listener as MCP Bridge Listener
    participant Agent
    participant Bridge as MCP Bridge Process

    Note over Conductor: Without oneshot channel coordination

    Conductor->>Listener: Spawn (no session_id yet)

    Conductor->>Agent: session/new request

    Note over Agent: Fast agent responds immediately

    Agent->>Bridge: Spawn bridge process

    Bridge->>Listener: TCP connect

    Note over Listener: ❌ No session_id available yet!<br/>Can't send McpConnectionReceived

    Agent-->>Conductor: session/new response {<br/>  session_id: "sess-abc"<br/>}

    Note over Conductor: Too late - connection already waiting

    rect rgb(200, 50, 50)
        Note over Listener,Bridge: Race condition:<br/>Connection arrived before session_id
    end
```

**Solution**: Listener blocks on oneshot channel:

```mermaid
sequenceDiagram
    participant Conductor
    participant Listener as MCP Bridge Listener
    participant Agent
    participant Bridge as MCP Bridge Process

    Conductor->>Listener: Spawn with oneshot receiver

    Note over Listener: Listening on TCP<br/>Waiting for session_id via oneshot

    Conductor->>Agent: session/new request

    Agent->>Bridge: Spawn bridge process

    Bridge->>Listener: TCP connect

    Note over Listener: Connection accepted<br/>⏸️ BLOCKS waiting for session_id

    Agent-->>Conductor: session/new response {<br/>  session_id: "sess-abc"<br/>}

    Conductor->>Listener: Send "sess-abc" via oneshot

    Note over Listener: ✅ Session ID received<br/>Unblocks with connection + session_id

    Listener->>Conductor: McpConnectionReceived {<br/>  acp_url: "acp:uuid-123",<br/>  session_id: "sess-abc"<br/>}

    rect rgb(50, 200, 50)
        Note over Listener,Bridge: No race condition:<br/>session_id always available
    end
```

**Key decision**: Block connection acceptance on session_id availability

- Listener accepts TCP connection immediately (agent won't wait)
- But blocks sending `McpConnectionReceived` until session_id arrives
- Guarantees session_id is always available when creating `_mcp/connect` request
- Simple implementation: `oneshot_rx.await?` before sending message

## Multiple MCP Servers

A single session can register multiple MCP servers:

```mermaid
flowchart TB
    Proxy[Proxy]
    Conductor[Conductor]
    Agent[Agent]

    Listener1[Listener: acp:uuid-1<br/>Port 54321]
    Listener2[Listener: acp:uuid-2<br/>Port 54322]

    Bridge1[Bridge: conductor mcp 54321]
    Bridge2[Bridge: conductor mcp 54322]

    Proxy -->|session/new with 2 servers| Conductor
    Conductor -->|session_id: sess-abc| Listener1
    Conductor -->|session_id: sess-abc| Listener2
    Conductor -->|session/new response| Proxy

    Agent <-->|stdio| Bridge1
    Agent <-->|stdio| Bridge2

    Bridge1 <-->|TCP| Listener1
    Bridge2 <-->|TCP| Listener2

    Listener1 -->|_mcp/* messages<br/>conn-1| Conductor
    Listener2 -->|_mcp/* messages<br/>conn-2| Conductor

    Conductor <-->|Both connections| Proxy

    style Listener1 fill:#e1f5ff
    style Listener2 fill:#e1f5ff
```

**Key decisions**:

- Each `acp:` URL gets its own TCP port and listener
- All listeners for a session receive the same session_id
- Each connection gets unique connection_id
- Proxy maintains map: `connection_id -> (session_id, acp_url)`

## Implementation Components

### McpBridgeListeners

- **Purpose**: Manages TCP listeners for all `acp:` URLs
- **Lifecycle**: Created with conductor, lives for entire conductor lifetime
- **Responsibilities**:
  - Detect `acp:` URLs during `session/new`
  - Spawn TCP listeners on ephemeral ports
  - Transform MCP server specs to stdio transport
  - Deliver session_id to listeners via oneshot channels

### McpBridgeListener Actor

- **Purpose**: Accepts TCP connections for a specific `acp:` URL
- **Lifecycle**: Spawned during `session/new`, lives until conductor exits
- **Responsibilities**:
  - Listen on TCP port
  - Block on oneshot channel to receive session_id
  - Accept connections and send `McpConnectionReceived` with session_id
  - Spawn connection actors

### McpBridgeConnectionActor

- **Purpose**: Routes messages for a single MCP connection
- **Lifecycle**: Spawned when agent connects, exits when agent disconnects
- **Responsibilities**:
  - Read JSON-RPC from TCP, forward to conductor
  - Receive messages from conductor, write to TCP
  - Send `McpConnectionDisconnected` on close

### MCP Bridge Process (`conductor mcp $PORT`)

- **Purpose**: Bridges agent's stdio to conductor's TCP
- **Lifecycle**: Spawned by agent, exits when stdio closes
- **Responsibilities**:
  - Connect to TCP port on startup
  - Bidirectional stdio ↔ TCP forwarding
  - No protocol awareness (just bytes)

## Error Handling

### Agent Disconnects During Session Creation

If agent closes connection before sending `session/new` response:

- Oneshot channel sender drops
- Listener receives `Err` from oneshot
- Listener exits gracefully
- TCP port cleaned up

### Bridge Process Crashes

If bridge process exits unexpectedly:

- TCP connection closes
- Listener detects disconnect
- Sends `McpConnectionDisconnected`
- Proxy cleans up state

### Multiple Connections to Same Listener

Decision: Allow multiple connections per listener (for future flexibility)

- Each connection gets unique connection_id
- All connections share same session_id
- Proxy can correlate all connections to session

## Related Documentation

- [Conductor Design](./conductor.md) - Conductor architecture
- [Protocol Reference](./protocol.md) - ACP message formats
- [Cookbook rustdoc](https://docs.rs/agent-client-protocol-cookbook) - Patterns for implementing MCP-aware proxies
