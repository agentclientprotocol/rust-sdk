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

2. **Transport Layer**: Message movement
   - Reading/writing from I/O sources
   - Serialization/deserialization
   - Connection management

This separation enables:

- **In-process efficiency**: Components in the same process can skip serialization
- **Transport flexibility**: Easy to add new transport types (WebSockets, named pipes, etc.)
- **Testability**: Mock transports for unit testing
- **Clarity**: Clear boundaries between protocol and I/O concerns

### The `TransportFrame` Boundary

The key insight is that `agent_client_protocol::RawJsonRpcMessage` provides a natural,
transport-neutral boundary backed by the JSON-RPC envelope types from
`agent-client-protocol-schema`:

```rust
enum RawJsonRpcMessage {
    Request(Request<RawJsonRpcParams>),
    Notification(Notification<RawJsonRpcParams>),
    Response(Response<serde_json::Value>),
}
```

`RawJsonRpcMessage` sits inside the transport-neutral public frame type:

- **Above**: Protocol layer works with application types (`OutgoingMessage`, `UntypedMessage`)
- **Below**: Transport layer parses and serializes `RawJsonRpcMessage`
- **Boundary**: `TransportFrame` carries one raw message, a structurally
  non-empty batch, or a malformed wire value retained for a relay
- **In-process API**: `Channel::rx` and `Channel::tx` carry `TransportFrame`
  directly, so adapters cannot accidentally flatten a batch
- **Failures**: I/O and connection failures are returned by the future driving
  a transport; they are not sent as channel entries

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
- Subscribe to reply_actor for response correlation
- Convert application-level `OutgoingMessage` to protocol-level `RawJsonRpcMessage`

#### Incoming Protocol Actor

```
Input:  mpsc::UnboundedReceiver<TransportFrame>
Output: Routes to reply_actor or registered handlers
```

Responsibilities:

- Route responses to reply_actor (matches by ID)
- Route requests/notifications to registered handlers
- Convert schema request/notification envelopes to `UntypedMessage` for handlers
- Retain batch response slots while entries are dispatched
- Emit one response array only after every response-bearing entry has completed
  and all entries in the batch have been dispatched
- Emit nothing for a notification-only batch; answer an empty batch with one
  standalone `Invalid Request` response

#### Reply Actor

Manages request/response correlation:

- Maintains map from request ID to response channel
- When response arrives, delivers to waiting request
- Unchanged from original design

#### Task Actor

Runs user-spawned concurrent tasks via `cx.spawn()`. Unchanged from original design.

### Transport Actors

These actors are driven by physical transport components and have **zero knowledge** of protocol semantics:

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
- Report malformed JSON once, and report invalid call-batch entries independently
- Ignore malformed response-batch entries instead of answering a response

For in-process channels:

- Directly forward `TransportFrame` from the channel

The public `Channel` boundary preserves complete frames. The SDK continues to
initiate requests and notifications as individual JSON-RPC messages; response
arrays are correlated replies to batch calls received from the peer. Relays and
instrumentation must forward frames intact so they do not change those wire
semantics.

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
    | - Route responses → reply_actor
    | - Route requests → registered handlers
    v
Handler or Reply Actor
```

### Message Ordering in the Conductor

When the conductor forwards messages between components, it must preserve send order to prevent race conditions. The conductor achieves this by routing all message forwarding through a central message queue.

**Key insight**: While the transport actors operate independently, the **conductor's routing logic** serializes all forwarding decisions through a central event loop. This ensures that even though responses use a "fast path" (reply_actor with oneshot channels) at the transport level, the decision to forward them is serialized with notification forwarding at the protocol level.

Without this serialization, responses could overtake notifications when both are forwarded through proxy chains, causing the client to receive messages out of order. See [Conductor Implementation](./conductor.md#message-ordering-invariant) for details.

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
- **Full type safety**: No parsing errors possible

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
