# Transport Architecture

For the broader user-facing API, see the [Core Library Design](./design.md) and
the [`agent-client-protocol` rustdoc](https://docs.rs/agent-client-protocol).

This chapter explains how the connection layer separates protocol semantics from transport mechanisms, enabling flexible deployment patterns including in-process message passing.

## Overview

The SDK's connection core provides the JSON-RPC abstraction used by ACP components. It supports **pluggable transports** that work with different I/O mechanisms while maintaining consistent protocol semantics.

## Design Principles

### Separation of Concerns

The architecture separates two distinct responsibilities:

1. **Protocol Layer**: JSON-RPC semantics
   - Request ID assignment
   - Request/response correlation
   - Method dispatch to handlers
   - Error handling

2. **Transport and framing layer**: Message movement and JSON-RPC envelope validation
   - Reading/writing from I/O sources
   - Serialization/deserialization
   - Preserving single-value and batch boundaries
   - Connection management

This separation enables:

- **In-process efficiency**: Components in the same process can skip serialization
- **Transport flexibility**: Easy to add new transport types (WebSockets, named pipes, etc.)
- **Testability**: Mock transports for unit testing
- **Clarity**: Clear boundaries between protocol and I/O concerns

### The `TransportFrame` Boundary

The public, transport-neutral boundary is `TransportFrame`. A frame contains
one `RawJsonRpcMessage`, one structurally non-empty `TransportBatch`, or a
malformed wire value that a relay must preserve. `RawJsonRpcMessage` is backed
by the JSON-RPC envelope types from `agent-client-protocol-schema`:

```rust
enum RawJsonRpcMessage {
    Request(Request<RawJsonRpcParams>),
    Notification(Notification<RawJsonRpcParams>),
    Response(Response<serde_json::Value>),
}
```

At that boundary:

- **Above**: Protocol layer works with application types (`OutgoingMessage`, `UntypedMessage`)
- **Below**: Transport actors parse and serialize JSON-RPC frames
- **Boundary**: `TransportFrame` carries one raw message, a structurally
  non-empty batch, or a malformed wire value retained for a relay
- **In-process API**: `Channel::rx` and `Channel::tx` carry `TransportFrame`
  directly, so adapters cannot accidentally flatten a batch
- **Failures**: I/O and connection failures are returned by the future driving
  a transport; they are not sent as channel entries

`TransportFrame::parse_json` returns one frame for every input string, including
malformed response-shaped input. Standalone malformed input retains its exact
text. Batch entries retain their parsed JSON values, source order, and batch
boundary, although serializing a relayed batch may normalize whitespace. The
protocol actor, not the parser, decides whether malformed input requires a
response.

## Actor Architecture

### Protocol Actors

These actors live in the protocol connection core and understand JSON-RPC semantics:

#### Outgoing Protocol Actor

```
Input:  mpsc::UnboundedReceiver<OutgoingMessage>
Output: mpsc::UnboundedSender<TransportFrame>
```

Responsibilities:

- Assign unique IDs to outgoing requests
- Register pending replies before sending requests
- Convert application-level `OutgoingMessage` to protocol-level `RawJsonRpcMessage`

#### Incoming Protocol Actor

```
Input:  mpsc::UnboundedReceiver<TransportFrame>
Output: Routes to pending request awaiters or registered handlers
```

Responsibilities:

- Route responses to pending request awaiters (matched by ID)
- Route requests/notifications to registered handlers
- Convert schema request/notification envelopes to `UntypedMessage` for handlers
- Retain batch response slots while entries are dispatched
- Emit one response array only after every response-bearing entry has completed
  and all entries in the batch have been dispatched
- Emit nothing for a notification-only batch; answer an empty batch with one
  standalone `Invalid Request` response

#### Pending Reply Registry

The shared pending-reply registry manages request/response correlation:

- Maintains map from request ID to response channel
- When response arrives, delivers to waiting request

#### Task Actor

Runs user-spawned concurrent tasks via `cx.spawn()`.

### Transport Actors

These actors are driven by physical transport components. They understand
JSON-RPC framing and envelope validity, but they do not dispatch ACP methods or
correlate responses with pending requests:

#### Transport Outgoing Actor

```
Input:  mpsc::UnboundedReceiver<TransportFrame>
Output: Writes to I/O (byte stream, channel, socket, etc.)
```

For byte streams:

- Serialize a single `RawJsonRpcMessage` or one non-empty batch to JSON
- Write newline-delimited JSON to stream

For in-process channels:

- Directly forward `TransportFrame` to the channel

#### Transport Incoming Actor

```
Input:  Reads from I/O (byte stream, channel, socket, etc.)
Output: mpsc::UnboundedSender<TransportFrame>
```

For byte streams:

- Read newline-delimited JSON from stream
- Parse a single message or a non-empty batch array
- Retain the batch boundary while dispatching each `RawJsonRpcMessage` entry to
  the incoming protocol actor
- Retain malformed entries so relays can forward the complete frame
- Leave call/response-shape classification and Error Response decisions to the
  incoming protocol actor

For in-process channels:

- Directly forward `TransportFrame` from the channel

The public `Channel` boundary preserves complete frames. The SDK continues to
initiate requests and notifications as individual JSON-RPC messages; response
arrays are correlated replies to batch calls received from the peer. Relays and
instrumentation must forward frames intact so they do not change those wire
semantics.

## JSON-RPC Batch Behavior

Batch support is shared by the stable v1 and draft v2 APIs because it belongs
to the JSON-RPC transport layer:

- `Lines`, `ByteStreams`, `Stdio`, and the HTTP/WebSocket adapters accept
  incoming JSON-RPC arrays.
- Entries are validated and dispatched independently and in source order. An
  invalid call-shaped entry receives its own `Invalid Request` error without
  preventing valid siblings from running.
- Responses for response-bearing entries are collected and written as one
  response array after dispatch completes. A notification-only batch receives
  no response.
- If a handler drops a batched request's `Responder`, the completed dispatch
  supplies an `Internal Error` for that slot so completed siblings are not
  stranded. Returning a handler error supplies that error instead. Dropping an
  individual request's responder continues to send no automatic response.
- An empty input array receives one standalone `Invalid Request` response; the
  SDK never writes an empty response array.
- Response entries are routed by request ID. The framing layer retains malformed
  values, but the protocol actor ignores values that are response-shaped and
  not call-shaped because a JSON-RPC response must not itself receive a
  response; ambiguous call-shaped values still receive `Invalid Request`.
- The SDK does not originate batches of requests or notifications. Individual
  calls continue to receive individual responses; a response array is emitted
  only for an incoming call batch.

Relays, wrappers, and tracing bridges must forward the complete
`TransportFrame`. Flattening a batch changes observable JSON-RPC semantics even
when every individual message remains valid.

Lifecycle-sensitive calls should normally be sent individually. As a
compatibility measure, `AgentProtocolRouter` can select a v1 or v2 agent when
the first call-shaped entry is `initialize`, while preserving the original
frame for the selected implementation. Response-only frames received before
initialization are ignored. `ClientProtocolConnector` starts each attempted
client implementation with an individual `initialize` request.

## Message Flow

### Outgoing Message Flow

```
User Handler
    |
    | OutgoingMessage (request/notification/response)
    v
Outgoing Protocol Actor
    | - Assign ID (for requests)
    | - Subscribe to replies
    | - Convert to RawJsonRpcMessage
    v
    | TransportFrame (single message or batch response)
    |
Transport Outgoing Actor
    | - Serialize (byte streams)
    | - Or forward directly (channels)
    v
I/O Destination
```

### Incoming Message Flow

```
I/O Source
    |
Transport Incoming Actor
    | - Parse (byte streams)
    | - Or forward directly (channels)
    v
    | TransportFrame (single message or incoming batch)
    |
Incoming Protocol Actor
    | - Route responses → pending request awaiters
    | - Route requests → registered handlers
    v
Handler or request awaiter
```

### Message Ordering in the Conductor

The conductor's central routing loop serializes forwarding decisions for
incoming requests and notifications. Responses stay paired with the request
contexts managed by the protocol layer and may take a direct response path.
The conductor therefore does not promise a global total order across unrelated
concurrent requests, but every underlying transport sink preserves the order in
which complete frames are accepted. See [Conductor Routing and
Ordering](./conductor.md#routing-and-ordering).

## Component Boundary

[`ConnectTo`](https://docs.rs/agent-client-protocol/latest/agent_client_protocol/trait.ConnectTo.html)
is the common component and transport abstraction. `connect_to` joins a
component to its counterpart and drives the connection until completion.
`into_channel_and_future` exposes the canonical low-level boundary as a
`Channel` plus the future that drives the component:

```rust,ignore
fn into_channel_and_future(self) -> (Channel, BoxFuture<'static, Result<()>>);
```

The returned future owns transport failures and lifecycle completion. The
channel carries only `TransportFrame` wire events. Most components implement
only `connect_to`; direct transports override `into_channel_and_future` to avoid
an intermediate copy.

## Transport Implementations

### Byte Stream Transport

`ByteStreams<Outgoing, Incoming>` works with `futures::io::AsyncWrite` and
`AsyncRead`. It adapts them to `Lines`, parses each incoming JSON value into one
frame, and serializes each outgoing frame to one newline-delimited JSON value.
`Stdio` and `AcpAgent` build on this transport.

Use cases:

- Stdio connections to subprocess agents
- TCP socket connections
- Unix domain sockets
- Any stream-based I/O

### In-Process Channel

For components in the same process, `Channel::duplex()` creates paired
endpoints and skips serialization entirely. Relays forward each received
`TransportFrame` without unpacking it; this preserves batch boundaries and the
original representation of malformed wire input.

Benefits:

- **Zero serialization overhead**: Messages passed by value
- **Same-process efficiency**: Ideal for conductor with in-process proxies
- **Explicit wire state**: No serialize/parse round trip is required, while a
  malformed value received from a physical transport remains an explicit frame

## Use Cases

### 1. Standard Agent (Stdio)

Use `Stdio` for the current process or `AcpAgent` for a child process. Both use
the same frame-aware line transport underneath.

### 2. In-Process Proxy Chain

Connect builders, proxies, and conductor components directly. Their default
`ConnectTo` adapter uses `Channel`, so complete frames cross each wrapper with
no serialization.

### 3. Network-Based Components

Split the socket and pass compatible read/write halves to `ByteStreams::new`.

### 4. Testing with Mock Transport

Use `Channel::duplex()` to inject and inspect `TransportFrame` values without
real I/O.

## Benefits

### Performance

- **In-process optimization**: Skip serialization when components are co-located
- **Zero-copy potential**: Direct message passing for channels
- **Flexible trade-offs**: Choose appropriate transport for deployment

### Flexibility

- **Transport-agnostic handlers**: Write handler logic once, use anywhere
- **Easy experimentation**: Try different transports without code changes
- **Future-proof**: Add new transports (WebSockets, gRPC, etc.) without refactoring

### Testing

- **Mock transports**: Unit test handlers without I/O
- **Deterministic tests**: Control message timing precisely
- **Isolated testing**: Test protocol logic separate from I/O

### Clarity

- **Clear boundaries**: Protocol semantics vs transport mechanics
- **Focused implementations**: Each layer has single responsibility
- **Maintainability**: Changes to transport don't affect protocol logic

## Related Documentation

- [Core Library Design](./design.md) - High-level crate architecture
- [Conductor Design](./conductor.md) - How the conductor uses transports
