# Transport Architecture

> **Note**: This document describes internal architecture and uses older terminology (e.g., `JrConnection` instead of the current API). For the user-facing API, see the [Core Library Design](./design.md) and the [`agent-client-protocol` rustdoc](https://docs.rs/agent-client-protocol).

This chapter explains how the connection layer separates protocol semantics from transport mechanisms, enabling flexible deployment patterns including in-process message passing.

## Overview

`JrConnection` provides the core JSON-RPC connection abstraction used by all ACP components. Originally designed around byte streams, it has been refactored to support **pluggable transports** that work with different I/O mechanisms while maintaining consistent protocol semantics.

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

### The `jsonrpcmsg::Message` Boundary

The key insight is that `jsonrpcmsg::Message` provides a natural, transport-neutral boundary:

```rust
enum jsonrpcmsg::Message {
    Request { method, params, id },
    Response { result, error, id },
}
```

This type sits between the protocol and transport layers:

- **Above**: Protocol layer works with application types (`OutgoingMessage`, `UntypedMessage`)
- **Below**: Transport layer works with `jsonrpcmsg::Message`
- **Boundary**: Clean, well-defined interface

## Actor Architecture

### Protocol Actors (Core JrConnection)

These actors live in `JrConnection` and understand JSON-RPC semantics:

#### Outgoing Protocol Actor

```
Input:  mpsc::UnboundedReceiver<OutgoingMessage>
Output: mpsc::UnboundedSender<jsonrpcmsg::Message>
```

Responsibilities:

- Assign unique IDs to outgoing requests
- Subscribe to reply_actor for response correlation
- Convert application-level `OutgoingMessage` to protocol-level `jsonrpcmsg::Message`

#### Incoming Protocol Actor

```
Input:  mpsc::UnboundedReceiver<jsonrpcmsg::Message>
Output: Routes to reply_actor or registered handlers
```

Responsibilities:

- Route responses to reply_actor (matches by ID)
- Route requests/notifications to registered handlers
- Convert `jsonrpcmsg::Request` to `UntypedMessage` for handlers

#### Reply Actor

Manages request/response correlation:

- Maintains map from request ID to response channel
- When response arrives, delivers to waiting request
- Unchanged from original design

#### Task Actor

Runs user-spawned concurrent tasks via `cx.spawn()`. Unchanged from original design.

### Transport Actors (Provided by Trait)

These actors are spawned by `IntoJrConnectionTransport` implementations and have **zero knowledge** of protocol semantics:

#### Transport Outgoing Actor

```
Input:  mpsc::UnboundedReceiver<jsonrpcmsg::Message>
Output: Writes to I/O (byte stream, channel, socket, etc.)
```

For byte streams:

- Serialize `jsonrpcmsg::Message` to JSON
- Write newline-delimited JSON to stream

For in-process channels:

- Directly forward `jsonrpcmsg::Message` to channel

#### Transport Incoming Actor

```
Input:  Reads from I/O (byte stream, channel, socket, etc.)
Output: mpsc::UnboundedSender<jsonrpcmsg::Message>
```

For byte streams:

- Read newline-delimited JSON from stream
- Parse to `jsonrpcmsg::Message`
- Send to incoming protocol actor

For in-process channels:

- Directly forward `jsonrpcmsg::Message` from channel

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
    | - Convert to jsonrpcmsg::Message
    v
    | jsonrpcmsg::Message
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
    | jsonrpcmsg::Message
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

## Transport Trait

The `IntoJrConnectionTransport` trait defines how to bridge internal channels with I/O:

```rust
pub trait IntoJrConnectionTransport {
    fn setup_transport(
        self,
        cx: &JrConnectionCx,
        outgoing_rx: mpsc::UnboundedReceiver<jsonrpcmsg::Message>,
        incoming_tx: mpsc::UnboundedSender<jsonrpcmsg::Message>,
    ) -> Result<(), Error>;
}
```

Key points:

- **Consumed** (`self`): Implementations move owned resources into spawned actors
- **Spawns via `cx.spawn()`**: Uses connection context to spawn transport actors
- **Channels only**: No knowledge of `OutgoingMessage` or response correlation
- **Returns quickly**: Just spawns actors, doesn't block

## Transport Implementations

### Byte Stream Transport

The default implementation works with any `AsyncRead` + `AsyncWrite` pair:

```rust
impl<OB: AsyncWrite, IB: AsyncRead> IntoJrConnectionTransport for (OB, IB) {
    fn setup_transport(self, cx, outgoing_rx, incoming_tx) -> Result<(), Error> {
        let (outgoing_bytes, incoming_bytes) = self;

        // Spawn incoming: read bytes → parse JSON → send Message
        cx.spawn(async move {
            let mut lines = BufReader::new(incoming_bytes).lines();
            while let Some(line) = lines.next().await {
                let message: jsonrpcmsg::Message = serde_json::from_str(&line?)?;
                incoming_tx.unbounded_send(message)?;
            }
            Ok(())
        });

        // Spawn outgoing: receive Message → serialize → write bytes
        cx.spawn(async move {
            while let Some(message) = outgoing_rx.next().await {
                let json = serde_json::to_vec(&message)?;
                outgoing_bytes.write_all(&json).await?;
                outgoing_bytes.write_all(b"\n").await?;
            }
            Ok(())
        });

        Ok(())
    }
}
```

Use cases:

- Stdio connections to subprocess agents
- TCP socket connections
- Unix domain sockets
- Any stream-based I/O

### In-Process Channel Transport

For components in the same process, skip serialization entirely:

```rust
pub struct ChannelTransport {
    outgoing: mpsc::UnboundedSender<jsonrpcmsg::Message>,
    incoming: mpsc::UnboundedReceiver<jsonrpcmsg::Message>,
}

impl IntoJrConnectionTransport for ChannelTransport {
    fn setup_transport(self, cx, outgoing_rx, incoming_tx) -> Result<(), Error> {
        // Just forward messages, no serialization
        cx.spawn(async move {
            while let Some(message) = self.incoming.next().await {
                incoming_tx.unbounded_send(message)?;
            }
            Ok(())
        });

        cx.spawn(async move {
            while let Some(message) = outgoing_rx.next().await {
                self.outgoing.unbounded_send(message)?;
            }
            Ok(())
        });

        Ok(())
    }
}
```

Benefits:

- **Zero serialization overhead**: Messages passed by value
- **Same-process efficiency**: Ideal for conductor with in-process proxies
- **Full type safety**: No parsing errors possible

## Construction API

### Flexible Construction

The refactored API separates handler setup from transport selection:

```rust
// Build connection with handlers
let connection = JrConnection::new()
    .name("my-component")
    .on_receive_request(|req: InitializeRequest, cx| {
        cx.respond(InitializeResponse::make())
    })
    .on_receive_notification(|notif: SessionNotification, _cx| {
        Ok(())
    });

// Provide transport at the end
connection.serve_with(transport).await?;
```

### Byte Stream Convenience

For the common case of byte streams, use the convenience constructor:

```rust
JrConnection::from_streams(stdout, stdin)
    .on_receive_request(...)
    .serve()
    .await?;
```

This is equivalent to:

```rust
JrConnection::new()
    .on_receive_request(...)
    .serve_with((stdout, stdin))
    .await?;
```

## Use Cases

### 1. Standard Agent (Stdio)

Traditional subprocess agent with stdio communication:

```rust
JrConnection::from_streams(
    tokio::io::stdout().compat_write(),
    tokio::io::stdin().compat()
)
    .name("my-agent")
    .on_receive_request(handle_prompt)
    .serve()
    .await?;
```

### 2. In-Process Proxy Chain

Conductor with proxies in the same process for maximum efficiency:

```rust
// Create paired channel transports
let (transport_a, transport_b) = create_paired_transports();

// Spawn proxy in background
tokio::spawn(async move {
    JrConnection::new()
        .on_receive_message(proxy_handler)
        .serve_with(transport_a)
        .await
});

// Connect to proxy
JrConnection::new()
    .on_receive_request(agent_handler)
    .serve_with(transport_b)
    .await?;
```

No serialization overhead between components!

### 3. Network-Based Components

TCP socket connections between components:

```rust
let stream = TcpStream::connect("localhost:8080").await?;
let (read, write) = stream.split();

JrConnection::new()
    .on_receive_request(handler)
    .serve_with((write.compat_write(), read.compat()))
    .await?;
```

### 4. Testing with Mock Transport

Unit tests without real I/O:

```rust
let (transport, mock) = create_mock_transport();

tokio::spawn(async move {
    JrConnection::new()
        .on_receive_request(my_handler)
        .serve_with(transport)
        .await
});

// Test by sending messages directly
mock.send_request("initialize", params).await?;
let response = mock.receive_response().await?;
assert_eq!(response.method, "initialized");
```

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

## Implementation Status

- ✅ **Phase 1**: Documentation complete
- 🚧 **Phase 2**: Actor splitting in progress
- 📋 **Phase 3**: Trait introduction planned
- 📋 **Phase 4**: In-process transport planned
- 📋 **Phase 5**: Conductor integration planned

## Related Documentation

- [Core Library Design](./design.md) - High-level crate architecture
- [Conductor Design](./conductor.md) - How the conductor uses transports
