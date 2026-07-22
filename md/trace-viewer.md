# Trace Viewer

`agent-client-protocol-trace-viewer` renders conductor message traces as an
interactive sequence diagram. Capture lives in
`agent-client-protocol-conductor`; the viewer can read the resulting file while
the conductor is running or after it exits.

## Capture and View

Conductor options are global and therefore precede the subcommand:

```bash
agent-client-protocol-conductor --trace ./trace.jsons agent \
  "proxy-one" "base-agent"

agent-client-protocol-trace-viewer ./trace.jsons
```

The standalone viewer chooses a loopback port and opens a browser. Use
`--port PORT` to choose the port or `--no-open` to suppress browser launch.

The conductor can also host the viewer directly:

```bash
# In-memory live trace
agent-client-protocol-conductor --serve agent "proxy-one" "base-agent"

# File-backed live trace
agent-client-protocol-conductor --trace ./trace.jsons --serve agent \
  "proxy-one" "base-agent"
```

Starting file capture truncates an existing trace file. Each event is flushed
as one JSON object followed by a newline.

## Event Schema

The `.jsons` file contains exactly three event variants: request, response, and
notification. It does not capture stderr or general tracing log records.

```typescript
type TraceEvent = RequestEvent | ResponseEvent | NotificationEvent;

interface RequestEvent {
  type: "request";
  ts: number;
  protocol: "acp" | "mcp";
  from: string;
  to: string;
  id: unknown;
  method: string;
  session?: string;
  params: unknown;
}

interface ResponseEvent {
  type: "response";
  ts: number;
  from: string;
  to: string;
  id: unknown;
  is_error: boolean;
  payload: unknown;
}

interface NotificationEvent {
  type: "notification";
  ts: number;
  protocol: "acp" | "mcp";
  from: string;
  to: string;
  method: string;
  session?: string;
  params: unknown;
}
```

`ts` is monotonic seconds since capture began. JSON-RPC IDs may be strings,
integers, or `null`; consumers must not assume they are numbers. The
optional `session` field is omitted when the tracer has no session context; it
is not serialized as `null`.

Components currently use the conductor's debug names, such as `Client`,
`Proxy(0)`, and `Agent`.

## Idealized Message Flow

The trace shows logical component-to-component traffic rather than conductor
plumbing:

- `_proxy/successor` is unwrapped and logged as its inner ACP method.
- `mcp/message` is unwrapped and its inner method is marked with protocol
  `mcp`.
- Responses are correlated with the request details retained by the trace
  writer.

The snooping bridges carry complete `TransportFrame` values. Enabling tracing
therefore preserves batch boundaries even though the viewer renders the
individual logical messages.

## Viewer Features

The web UI provides ACP/MCP/response filters, request-response color pairing,
active-request spans, elapsed-time labels, `session/update` text previews, a
resizable JSON detail panel, and adjustable swimlane width. The file-backed
server rereads the trace on each poll so new events appear during capture.

## Programmatic Capture

```rust,ignore
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};

let conductor = ConductorImpl::new_agent(
    "conductor",
    ProxiesAndAgent::new(agent).proxy(proxy),
)
.trace_to_path("./trace.jsons")?;

conductor.run(upstream_transport).await?;
```

Use `trace_to` with a custom implementation of
`agent_client_protocol_conductor::trace::WriteEvent` to send events somewhere
other than a file. `with_trace_writer` accepts an already configured
`TraceWriter`.

The viewer crate also exposes `serve_file` and `serve_memory`. The memory-backed
form returns a `TraceHandle` for pushing JSON events and a server future that
the caller must drive.
