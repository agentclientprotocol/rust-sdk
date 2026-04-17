# Trace Viewer

`agent-client-protocol-trace-viewer` is a debugging tool that visualizes message flow between ACP components as an interactive sequence diagram.

## Overview

When debugging ACP proxy chains, understanding the flow of messages between components is critical. The trace viewer provides:

- **Visual sequence diagrams** showing messages flowing between Client, Proxies, and Agent
- **Request/response correlation** with rainbow colors linking each request to its response
- **Timeline spans** showing when components are actively processing requests
- **Delta times** on each component's timeline showing elapsed time between events
- **Content preview** for `session/update` notifications showing message text inline
- **Filter controls** to show/hide ACP, MCP, and response messages
- **Adjustable layout** with a slider to control swimlane width

## Architecture

The system has two parts:

1. **Event capture** (in `agent-client-protocol-conductor`) - appends structured JSON events to a `.jsons` file
2. **Viewer** (in `agent-client-protocol-trace-viewer`) - serves an interactive HTML/JS visualization

### Workflow

```bash
# Run conductor with tracing enabled
agent-client-protocol-conductor agent --trace ./trace.jsons "proxy1" "proxy2" "agent"

# View the trace (works during or after the run)
agent-client-protocol-trace-viewer ./trace.jsons
# Opens browser to http://localhost:PORT
```

## Event Schema

Events are stored as newline-delimited JSON (`.jsons` file). Each line is a self-contained event.

### Common Fields

All events share these fields:

```typescript
interface BaseEvent {
  // Monotonic timestamp (seconds since trace start, f64)
  ts: number;

  // Event type discriminator
  type: "request" | "response" | "notification" | "trace";
}
```

### Request Event

A JSON-RPC request from one component to another.

```typescript
interface RequestEvent extends BaseEvent {
  type: "request";

  // Protocol: "acp" for ACP messages, "mcp" for MCP-over-ACP messages
  protocol: "acp" | "mcp";

  // Source component (e.g., "client", "proxy:0", "proxy:1", "agent")
  from: string;

  // Destination component
  to: string;

  // JSON-RPC request ID (for correlating with response)
  id: number | string;

  // JSON-RPC method name
  method: string;

  // ACP session ID, if known (null before session/new completes)
  session: string | null;

  // Full request params (may be large)
  params: object;
}
```

### Response Event

A JSON-RPC response to a prior request.

```typescript
interface ResponseEvent extends BaseEvent {
  type: "response";

  // Source component (who sent the response)
  from: string;

  // Destination component (who receives the response)
  to: string;

  // JSON-RPC request ID this responds to
  id: number | string;

  // True if this is an error response
  is_error: boolean;

  // Response result or error object
  payload: object;
}
```

### Notification Event

A JSON-RPC notification (no response expected).

```typescript
interface NotificationEvent extends BaseEvent {
  type: "notification";

  // Protocol: "acp" for ACP messages, "mcp" for MCP-over-ACP messages
  protocol: "acp" | "mcp";

  from: string;
  to: string;
  method: string;
  session: string | null;
  params: object;
}
```

### Trace Event

A tracing log line from a component (captured from stderr or tracing subscriber).

```typescript
interface TraceEvent extends BaseEvent {
  type: "trace";

  // Which component emitted this log
  component: string;

  // Log level
  level: "trace" | "debug" | "info" | "warn" | "error";

  // Log message
  message: string;

  // Optional structured fields from tracing spans
  fields?: Record<string, unknown>;
}
```

## Component Naming

Components are identified by strings:

- `"client"` - the upstream client (editor/IDE)
- `"proxy:0"`, `"proxy:1"`, etc. - proxy components by index
- `"agent"` - the final agent component

The viewer displays these as swimlane columns in the sequence diagram.

## Viewer Features

### Sequence Diagram

The main view is a vertical timeline with component columns (swimlanes):

```
       client     proxy:0         agent
         │           │               │
         │──init────>│    14ms       │
         │   97ms    │──init────────>│
         │           │      74ms     │<── orange (request/response pair)
         │           │<╌╌╌╌╌╌╌╌╌╌╌╌╌╌│
         │<╌╌╌╌╌╌╌╌╌╌│               │
         │           │               │
         │──prompt──>│               │
         │           ║    15ms       │<── thickened timeline = processing
         │           │──prompt──────>│
         │           │      235ms    │
         │           │<──tools/list──●    (MCP: circular arrowhead)
         │           │╌╌╌╌╌╌╌╌╌╌╌╌╌╌>│
         │           │               │
```

### Visual Design

- **Requests/notifications** - solid arrows with method name label, colored by request/response pair
- **Responses** - dashed arrows in the same color as their originating request
- **MCP vs ACP** - MCP messages use circular arrowheads; ACP uses triangular
- **Timeline spans** - when a component receives a request, its timeline thickens (in the pair's color) until the response is sent
- **Delta times** - elapsed time shown on each component's timeline between its events
- **Content preview** - `session/update` notifications show truncated message content below the arrow
- **Error responses** - highlighted in red

### Interactions

- **Click message** - expands full JSON payload in resizable bottom panel
- **Filter checkboxes** - show/hide ACP messages, MCP messages, responses
- **Width slider** - adjust swimlane column width (80-300px)

### Color Coding

Each request/response pair gets a unique color from a rainbow palette. This makes it easy to visually trace a request through the proxy chain to its response, even when many messages are interleaved.

## Implementation Notes

### Idealized View

The trace shows the _logical_ message flow between components, hiding conductor implementation details. The conductor transforms internal messages before logging:

| Internal Message                       | Logged As                                       |
| -------------------------------------- | ----------------------------------------------- |
| `_proxy/successor/foo` from proxy N    | `foo` from proxy N to proxy N+1 (protocol: acp) |
| `_mcp/request` with inner method `bar` | `bar` from agent to proxy (protocol: mcp)       |
| `_mcp/notify` with inner method `baz`  | `baz` from agent to proxy (protocol: mcp)       |

This means the viewer shows what conceptually happened without exposing the routing machinery.

### agent-client-protocol-conductor changes

**CLI usage:**

```bash
agent-client-protocol-conductor agent --trace ./trace.jsons "proxy1" "proxy2" "agent"
```

**Programmatic usage:**

```rust
Conductor::new(name, components, mcp_bridge_mode)
    .trace_to("./trace.jsons")
    .run(transport)
    .await
```

**Implementation:**

- Capture happens in `handle_conductor_message` where all messages flow
- Conductor transforms messages to idealized form before logging
- Events appended as JSON lines (one `writeln!` per event)
- File opened in append mode, flushed after each event

### agent-client-protocol-trace-viewer crate

- Single binary with embedded static assets (HTML/JS/CSS via `include_str!`)
- Axum HTTP server with two endpoints:
  - `GET /` - serves the viewer HTML (single-page app)
  - `GET /events` - returns trace events as JSON array
- Vanilla JS renders SVG sequence diagram (no build step, no npm)
- Auto-opens browser on launch (disable with `--no-open`)
