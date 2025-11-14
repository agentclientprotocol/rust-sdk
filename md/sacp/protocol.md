# Protocol Reference

This chapter documents the SACP protocol extensions to ACP. These extensions use ACP's [extensibility mechanism](https://agentclientprotocol.com/protocol/extensibility) through custom methods and `_meta` fields.

## Overview

SACP defines two main protocol extensions:

1. **`_proxy/successor/*`** - For proxy-to-successor communication
2. **`_mcp/*`** - For MCP-over-ACP bridging

## The `_proxy/successor/*` Protocol

Proxies communicate with their downstream component (next proxy or agent) through the conductor using these extension methods.

### `_proxy/successor/request`

Send a request to the successor component.

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_proxy/successor/request",
  "params": {
    // The actual ACP request to forward, flattened
    "method": "prompt",
    "params": {
      "messages": [...]
    }
  }
}
```

**Response:**
The response is the successor's response to the forwarded request:
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    // The successor's response
  }
}
```

**Usage:** When a proxy receives an ACP request from upstream and wants to forward it (possibly transformed) to the downstream component, it sends `_proxy/successor/request` to the conductor. The conductor routes it to the next component.

### `_proxy/successor/notification`

Send a notification to the successor component.

**Notification:**
```json
{
  "jsonrpc": "2.0",
  "method": "_proxy/successor/notification",
  "params": {
    // The actual ACP notification to forward, flattened
    "method": "cancelled",
    "params": {}
  }
}
```

**Usage:** When a proxy receives a notification from upstream and wants to forward it downstream.

### Message Flow Examples

**Example: Transforming a prompt**

1. Editor sends `prompt` request to conductor
2. Conductor forwards as normal ACP `prompt` to Proxy A
3. Proxy A modifies the prompt and sends:
   ```json
   {
     "method": "_proxy/successor/request",
     "params": {
       "method": "prompt",
       "params": { /* modified prompt */ }
     }
   }
   ```
4. Conductor routes to Proxy B as normal `prompt`
5. Response flows back through the chain

**Example: Pass-through proxy**

A minimal proxy that just forwards everything:

```rust
use sacp_proxy::{JsonRpcCxExt, AcpProxyExt};

// Forward request downstream
cx.send_request_to_successor(request)
  .await_when_result_received(|result| {
    cx.respond_with_result(result)
  })

// Forward notification downstream  
cx.send_notification_to_successor(notification)
```

## Capability Handshake

### The `Proxy` Capability

The conductor uses a two-way capability handshake to verify components can act as proxies.

**InitializeRequest from conductor to proxy:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "protocolVersion": "0.7.0",
    "capabilities": {},
    "_meta": {
      "proxy": true
    }
  }
}
```

**InitializeResponse from proxy to conductor:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": "0.7.0",
    "serverInfo": {},
    "capabilities": {},
    "_meta": {
      "proxy": true
    }
  }
}
```

**Why a two-way handshake?**

The proxy capability is an *active protocol* - it requires the component to handle `_proxy/successor/*` messages and route communications. If a component doesn't respond with the proxy capability, the conductor fails initialization with an error.

**Agent initialization:**

The last component (agent) is NOT offered the proxy capability:
```json
{
  "method": "initialize",
  "params": {
    "protocolVersion": "0.7.0",
    "capabilities": {},
    "_meta": {}  // No proxy capability
  }
}
```

Agents don't need SACP awareness.

## The `_mcp/*` Protocol

SACP enables components to provide MCP servers that communicate over ACP messages instead of stdio.

### MCP Server Declaration

Components declare MCP servers with ACP transport using a special URL scheme:

```json
{
  "tools": {
    "mcpServers": {
      "sparkle": {
        "transport": "http",
        "url": "acp:550e8400-e29b-41d4-a716-446655440000",
        "headers": {}
      }
    }
  }
}
```

The `acp:UUID` URL signals ACP transport. The component generates a unique UUID to identify which component handles calls to this MCP server.

### `_mcp/connect`

Create a new MCP connection (equivalent to "running the command").

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_mcp/connect",
  "params": {
    "acp_url": "acp:550e8400-e29b-41d4-a716-446655440000"
  }
}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "connection_id": "conn-123"
  }
}
```

The `connection_id` is used in subsequent MCP messages to identify which connection.

### `_mcp/disconnect`

Disconnect an MCP connection.

**Notification:**
```json
{
  "jsonrpc": "2.0",
  "method": "_mcp/disconnect",
  "params": {
    "connection_id": "conn-123"
  }
}
```

### `_mcp/request`

Send an MCP request over the ACP connection. This is bidirectional:
- Agent→Component: MCP client calling MCP server (tool calls, resource reads, etc.)
- Component→Agent: MCP server calling MCP client (sampling/createMessage, etc.)

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_mcp/request",
  "params": {
    "connection_id": "conn-123",
    // The actual MCP request, flattened
    "method": "tools/call",
    "params": {
      "name": "embody_sparkle",
      "arguments": {}
    }
  }
}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    // The MCP response
    "content": [
      {"type": "text", "text": "Embodiment complete"}
    ]
  }
}
```

### `_mcp/notification`

Send an MCP notification over the ACP connection. Bidirectional like `_mcp/request`.

**Notification:**
```json
{
  "jsonrpc": "2.0",
  "method": "_mcp/notification",
  "params": {
    "connection_id": "conn-123",
    // The actual MCP notification, flattened
    "method": "notifications/progress",
    "params": {
      "progressToken": "token-1",
      "progress": 50,
      "total": 100
    }
  }
}
```

### Agent Capability: `mcp_acp_transport`

Agents that natively support MCP-over-ACP declare this capability:

```json
{
  "_meta": {
    "mcp_acp_transport": true
  }
}
```

**Conductor behavior:**
- If the agent has `mcp_acp_transport: true`, conductor passes MCP server declarations through unchanged
- If the agent lacks this capability, conductor performs **bridging adaptation**:
  1. Binds a TCP port (e.g., `localhost:54321`)
  2. Transforms MCP server to use `conductor mcp PORT` command with stdio transport
  3. Spawns bridge process that converts between stdio (MCP) and ACP messages
  4. Agent thinks it's talking to normal MCP server over stdio

**Bridging transformation example:**

Original (from component):
```json
{
  "sparkle": {
    "transport": "http",
    "url": "acp:550e8400-e29b-41d4-a716-446655440000"
  }
}
```

Transformed (for agent without native support):
```json
{
  "sparkle": {
    "command": "conductor",
    "args": ["mcp", "54321"],
    "transport": "stdio"
  }
}
```

The `conductor mcp PORT` process bridges between stdio and the conductor's ACP message routing.

## Message Direction Summary

| Message | Direction | Purpose |
|---------|-----------|---------|
| `_proxy/successor/request` | Proxy→Conductor | Forward request downstream |
| `_proxy/successor/notification` | Proxy→Conductor | Forward notification downstream |
| `_mcp/connect` | Agent↔Component | Establish MCP connection |
| `_mcp/disconnect` | Agent↔Component | Close MCP connection |
| `_mcp/request` | Agent↔Component | Bidirectional MCP requests |
| `_mcp/notification` | Agent↔Component | Bidirectional MCP notifications |

## Building on SACP

When building proxies, you use the `sacp-proxy` crate which provides:

- `AcpProxyExt` trait for handling successor messages
- `JsonRpcCxExt` trait for sending to successor
- `ProxyHandler` for automatic proxy capability handshake

See [Building a Proxy](./building-proxy.md) for implementation guide.
