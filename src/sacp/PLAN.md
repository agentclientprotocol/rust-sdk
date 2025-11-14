# JrConnection Pluggable Transport Refactoring

## Motivation

Currently, `JrConnection` is hardcoded to work with byte streams (`AsyncWrite` + `AsyncRead`). This creates several limitations:

1. **In-process overhead**: When conductor and proxies run in the same process, we still serialize/deserialize messages through byte streams
2. **Inflexible construction**: Transport mechanism must be specified upfront when creating `JrConnection`
3. **Hard to abstract**: Difficult to package proxies/agents as trait implementations that work across different transports

## Goals

- Decouple transport mechanism from handler setup
- Enable in-process message passing without serialization overhead  
- Allow proxies/conductors to be packaged as trait implementations
- Make `AcpAgent` usable as a transport
- Keep all request/response correlation logic in core, not in transports

## Design

### Key Insight: Protocol vs Transport Separation

The current architecture conflates two responsibilities:
- **Protocol layer**: Managing JSON-RPC semantics (request IDs, response correlation, method dispatch)
- **Transport layer**: Moving bytes or messages between processes

We separate these by introducing a clean boundary at `jsonrpcmsg::Message`.

### Actor Architecture

**Before (current):**
```
[Handler] ← [incoming_actor: parsing + routing] ← [bytes]
[Handler] → [outgoing_actor: ID assignment + serialization] → [bytes]
```

**After (refactored):**
```
[Handler] ← [incoming_protocol] ← [transport_incoming] ← [bytes/messages]
[Handler] → [outgoing_protocol] → [transport_outgoing] → [bytes/messages]
```

### Protocol Actors (Core JrConnection)

These actors understand JSON-RPC semantics and manage the protocol state:

1. **Outgoing Protocol Actor**
   - Input: `mpsc::UnboundedReceiver<OutgoingMessage>`
   - Output: `mpsc::UnboundedSender<jsonrpcmsg::Message>`
   - Responsibilities:
     - Assign unique IDs to outgoing requests
     - Subscribe to replies via reply_actor
     - Convert `OutgoingMessage` → `jsonrpcmsg::Message`

2. **Incoming Protocol Actor**
   - Input: `mpsc::UnboundedReceiver<jsonrpcmsg::Message>`
   - Output: Routes to reply_actor or handler
   - Responsibilities:
     - Route responses to reply_actor (for request/response correlation)
     - Route requests/notifications to handler chain
     - Convert `jsonrpcmsg::Request` → `UntypedMessage` → dispatch

3. **Reply Actor** (unchanged)
   - Manages request/response correlation map
   - Matches responses to waiting requests by ID

4. **Task Actor** (unchanged)
   - Runs user-spawned concurrent tasks

### Transport Actors (Provided by Trait Implementation)

These actors only understand `jsonrpcmsg::Message` and have zero knowledge of protocol semantics:

1. **Transport Outgoing Actor**
   - Input: `mpsc::UnboundedReceiver<jsonrpcmsg::Message>`
   - Output: Writes to I/O (byte stream, TCP socket, channel, etc.)
   - For byte streams: Serializes to JSON and writes
   - For in-process: Directly forwards to channel

2. **Transport Incoming Actor**
   - Input: Reads from I/O (byte stream, TCP socket, channel, etc.)
   - Output: `mpsc::UnboundedSender<jsonrpcmsg::Message>`
   - For byte streams: Parses JSON from bytes
   - For in-process: Directly forwards from channel

### Transport Trait

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

Implementations spawn their transport actors using `cx.spawn()`. The trait consumes `self` to allow moving owned resources (like streams) into the spawned actors.

### Example Implementations

**Byte Streams:**
```rust
impl<OB: AsyncWrite, IB: AsyncRead> IntoJrConnectionTransport for (OB, IB) {
    fn setup_transport(self, cx, outgoing_rx, incoming_tx) -> Result<(), Error> {
        let (outgoing_bytes, incoming_bytes) = self;
        
        // Spawn transport incoming: bytes → parse → jsonrpcmsg::Message
        cx.spawn(async move {
            // Read lines, parse JSON, send to incoming_tx
        });
        
        // Spawn transport outgoing: jsonrpcmsg::Message → serialize → bytes
        cx.spawn(async move {
            // Read from outgoing_rx, serialize, write bytes
        });
        
        Ok(())
    }
}
```

**In-Process Channels:**
```rust
struct ChannelTransport {
    incoming_messages: mpsc::Receiver<jsonrpcmsg::Message>,
    outgoing_messages: mpsc::Sender<jsonrpcmsg::Message>,
}

impl IntoJrConnectionTransport for ChannelTransport {
    fn setup_transport(self, cx, outgoing_rx, incoming_tx) -> Result<(), Error> {
        // Just forward messages directly, no serialization
        cx.spawn(async move {
            // Forward from outgoing_rx to self.outgoing_messages
        });
        cx.spawn(async move {
            // Forward from self.incoming_messages to incoming_tx
        });
        Ok(())
    }
}
```

### Construction API Changes

**Before:**
```rust
JrConnection::new(outgoing_bytes, incoming_bytes)
    .on_receive_request(...)
    .serve()
```

**After:**
```rust
// Flexible construction
JrConnection::new()
    .on_receive_request(...)
    .serve_with(transport)

// Convenience for byte streams
JrConnection::from_streams(outgoing, incoming)
    .on_receive_request(...)
    .serve()
```

## Implementation Plan

### Phase 1: Documentation ✅
- [x] Create this PLAN.md
- [x] Update mdbook with architecture documentation (`md/transport-architecture.md`)

### Phase 2: Split Actors (No API Changes) ✅
Split existing actors into protocol/transport layers while keeping the same external API:

- [x] Create `outgoing_protocol_actor` that converts `OutgoingMessage` → `jsonrpcmsg::Message`
- [x] Create `incoming_protocol_actor` that routes `jsonrpcmsg::Message`
- [x] Extract transport logic from current `incoming_actor` → `transport_incoming_actor`
- [x] Extract transport logic from current `outgoing_actor` → `transport_outgoing_actor`
- [x] Update `serve()` to spawn all four actors with internal channels
- [x] Run tests to ensure no regressions (all 45+ tests pass)
- [x] Remove old monolithic actor functions
- [x] Commit: `refactor(sacp): split actors into protocol and transport layers`

**Success criteria**: All existing tests pass, no API changes, refactored internals. ✅

### Phase 3: Introduce Transport Trait
Add the trait and new construction patterns:

- [ ] Add `IntoJrConnectionTransport` trait to `jsonrpc.rs`
- [ ] Implement trait for `(AsyncWrite, AsyncRead)`
- [ ] Add `serve_with(transport)` method
- [ ] Add `from_streams()` convenience constructor
- [ ] Update `serve()` to use `serve_with()` internally
- [ ] Update `with_client()` similarly
- [ ] Run tests to ensure compatibility

**Success criteria**: Can construct `JrConnection` either way, all tests pass.

### Phase 4: In-Process Transport
Create channel-based transport implementation:

- [ ] Create `ChannelTransport` struct
- [ ] Implement `IntoJrConnectionTransport` for it
- [ ] Add helper for creating paired transports
- [ ] Write tests for in-process communication
- [ ] Document usage patterns

### Phase 5: Update Conductor/Proxy
Apply the new pattern to conductor and proxy:

- [ ] Update conductor to support in-process proxy chains
- [ ] Provide both byte-stream and channel-based modes
- [ ] Update examples
- [ ] Document the new patterns

## Benefits

1. **Performance**: In-process conductor/proxy chains skip serialization entirely
2. **Flexibility**: Easy to add new transport types (WebSockets, named pipes, etc.)
3. **Testability**: Mock transports for unit testing without I/O
4. **Composability**: Build handler chains independently of transport
5. **Clarity**: Clear separation of protocol semantics from transport mechanics

## Open Questions

- [ ] Should `jsonrpcmsg` dependency be exposed in public API? (Decision: Yes, it's fine)
- [ ] Should transport trait be `Into` (consumed) or borrowed? (Decision: Into/consumed)
- [ ] Keep backward compatibility or allow breaking changes? (TBD based on semver)

## Progress Tracking

- **Started**: 2025-11-05
- **Current Phase**: Phase 3 - Trait Introduction
- **Last Updated**: 2025-11-05

## Commit History

- `refactor(sacp): split actors into protocol and transport layers` (2db7e1d) - Phase 2 complete
