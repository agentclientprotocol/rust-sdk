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
   - Preserving each wire value, including malformed input
   - Connection management

This separation enables:

- **In-process efficiency**: Components in the same process can skip serialization
- **Transport flexibility**: Easy to add new transport types (WebSockets, named pipes, etc.)
- **Testability**: Mock transports for unit testing
- **Clarity**: Clear boundaries between protocol and I/O concerns

### The `TransportFrame` Boundary

The public, transport-neutral boundary is `TransportFrame`. A frame contains
one `RawJsonRpcMessage` or a malformed wire value that a relay must preserve.
`RawJsonRpcMessage` is the same for every ACP protocol version:

```rust
enum RawJsonRpcMessage {
    Request(RawJsonRpcRequest),
    Notification(Notification<RawJsonRpcParams>),
    Response(RawJsonRpcResponse),
}
```

Requests and responses carry a `RawJsonRpcId`: the JSON-RPC `id` exactly as
it appears on the wire (an integer, a string or `null`). Each protocol
version's schema has its own request ID type, and converts to and from it.

At that boundary:

- **Above**: Protocol layer works with application types (`OutgoingMessage`, `UntypedMessage`)
- **Below**: Transport actors parse and serialize JSON-RPC frames
- **Boundary**: `TransportFrame` carries one raw message or a malformed wire
  value retained for a relay
- **In-process API**: `Channel::rx` and `Channel::tx` carry `TransportFrame`
  directly
- **Failures**: I/O and connection failures are returned by the future driving
  a transport; they are not sent as channel entries

`TransportFrame::parse_json` returns one frame for every input string, including
malformed response-shaped input. Malformed input retains its exact text. A JSON
array is a malformed frame with an `Invalid Request` error, since JSON-RPC
batches are not supported (see [JSON-RPC Batches](#json-rpc-batches)). The
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
- Answer malformed call-shaped input, including any JSON array, with one error
  response with `id: null`

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

- Serialize a single `RawJsonRpcMessage` to JSON, or write a relayed malformed
  value unchanged
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
- Parse a single message
- Retain malformed input, including JSON arrays, so relays can forward it
- Leave call/response-shape classification and Error Response decisions to the
  incoming protocol actor

For in-process channels:

- Directly forward `TransportFrame` from the channel

The public `Channel` boundary preserves complete frames. The SDK sends every
request, notification, and response as an individual JSON-RPC message. Relays
and instrumentation must forward frames intact so they do not change wire
semantics.

## JSON-RPC Batches

The SDK does not support JSON-RPC batching, in either protocol version. ACP
protocol v2 removed batches, and v1 support was experimental and best-effort.

- `Lines`, `ByteStreams`, `Stdio`, and in-process channels parse an incoming
  JSON array, including an empty one, as one malformed frame. The connection
  answers it with a single `Invalid Request` (`-32600`) error with `id: null`
  and processes none of its entries. The connection stays usable.
- The HTTP transport answers a `POST` body that is a JSON array with
  `400 Bad Request` and that JSON-RPC error as the body. The WebSocket transport
  answers an array frame with that error. Neither forwards any entry to the
  agent.
- `AgentProtocolRouter` answers a batched first `initialize` the same way and
  keeps the connection open, so the client can send `initialize` again.
- The SDK never sends a batch.

Since the removal, `TransportFrame` has only the `Single` and `Malformed`
variants, and `TransportBatch` and `TransportBatchEntry` no longer exist. Code
that matched `TransportFrame::Batch` or built batches should send each message
as its own `TransportFrame::Single`. A dropped `Responder` is answered with an
automatic error; see
[Request Cancellation](./request-cancellation.md#unanswered-requests).

Lifecycle-sensitive routing is unaffected: `ProxyProtocolRouter` negotiates
`proxy/initialize` like `AgentProtocolRouter` negotiates `initialize` and does
not convert traffic after initialization, and `ClientProtocolConnector` starts each attempted
client implementation with an individual `initialize` request. Response-only
frames received before initialization are ignored.

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
    | TransportFrame (single message)
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
    | TransportFrame (single message or malformed input)
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
`Channel` plus an explicit connection driver:

```rust,ignore
fn into_channel_and_future(self) -> (Channel, Option<ConnectionDriver>);
```

The channel carries only `TransportFrame` wire events. The optional driver
distinguishes owned work from a passive endpoint:

| Returned work | Lifetime rule |
| --- | --- |
| `Some(ConnectionDriver::new(future))` | Poll the owned work alongside traffic; successful completion ends it after accepted output is drained. |
| `None` | No work is owned here; each channel half determines its own lifetime. |

A `ConnectionDriver` always contains a real future; the optional return value
cannot itself be awaited. There is no ready-successful passive driver and no
finish hook in the `None` case. This makes the ownership decision explicit
rather than requiring callers to recognize a special future.

A raw `Channel` returns `None`. Its bridge preserves both directions
independently:
one sender closing must not prevent a final response in the reverse direction.
Owned completion lets a bridge stop accepting new output, drain frames already
accepted, and finish without waiting for unrelated remote input to close.
Outbound forwarding must remain polled while owned work is running; otherwise
a component waiting for a response to its own request could deadlock.

Buffered adapters are responsible for flushing their accepted output before
reporting successful completion. The built-in line and byte-stream adapters
keep the read half moving during write drain and propagate incoming errors;
their explicit finish handling does not require remote read EOF.
`ConnectionDriver::with_finish(future, finish)` lets custom adapters declare
the same cooperative contract. Its one-shot, nonblocking hook requests finish
after protocol output handoff; the still-polled future performs the drain and
reports completion or I/O errors. `request_finish()` exposes this request to
low-level callers and is idempotent: a supported request remains supported
after it has been issued. The callback still runs at most once. Neither
requesting finish nor dropping the hook proves a successful flush, and no
implicit timeout is imposed.

`ConnectionDriver::new(future)` remains appropriate for opaque cancellable
work. A finite foreground does not wait indefinitely for such work after
handing off protocol output. Merely wrapping an arbitrary future cannot make
an opaque custom adapter drain safely.

Most components implement only `connect_to`; default normalization supplies
`Some(driver)` containing the owned work. Direct transports override
`into_channel_and_future` to avoid an intermediate copy. Wrappers that expose an
existing endpoint should forward
its driver unchanged so absence and cooperative completion handling are
not lost. Wrappers that decorate execution use `map_future` to transform the
owned future while preserving its finish capability and requested state.
Direct custom transport entry points must also request and await cooperative
finish after a finite peer completes; a bare join does not provide that step.

See [Migrating Connection Drivers](./migration-connection-drivers.md) for custom
override changes. This lifecycle distinction does not change the existing raw
channel types or introduce frame-size, queue, or task limits.

## Transport Implementations

### Byte Stream Transport

`ByteStreams<Outgoing, Incoming>` works with `futures::io::AsyncWrite` and
`AsyncRead`. It adapts them to `Lines`, parses each incoming JSON value into one
frame, and serializes each outgoing frame to one newline-delimited JSON value.
Runtime adapters with different I/O traits can instead bridge asynchronous
lines into `Lines`.

The native `Stdio` and `AcpAgent` transports build on `ByteStreams` and require
the independent `stdio` and `process` features, respectively. No features are
enabled by default. Their implementations depend on process spawning and
blocking-thread facilities, so their dependencies and exports remain excluded
on WebAssembly even when enabled. The runtime-neutral
protocol engine and transport abstractions compile for `wasm32-wasip1` and
`wasm32-wasip2` without additional features. JavaScript-hosted
`wasm32-unknown-unknown` builds require the opt-in `wasm_js` feature, which
selects Web Crypto through `wasm-bindgen` as the UUID randomness backend. Since
the target does not imply a JavaScript host, other OS-less WebAssembly hosts
must arrange a compatible UUID randomness backend instead. This crate does not
provide a WebAssembly executor or host I/O adapter.

Handlers, transports, and connection futures must be `Send` on every target,
including WebAssembly. JavaScript host values such as `wasm_bindgen::JsValue`
are `!Send`, so keep them outside the connection and exchange `Send` data with
it through `Channel` or `ByteStreams`. A later release may relax these bounds
on WebAssembly. That change would not affect native code.

The rmcp integration library also compiles for WASI with its `rmcp_3` feature,
alone or with its MCP-over-ACP features. It uses Tokio's async I/O utilities and
single-thread-capable runtime without requiring native stdio or a multithreaded
executor. Embedders still provide their runtime and host transport. See
[Cargo Features](./features.md) for configuration and migration examples.

Use cases:

- Stdio connections to subprocess agents
- TCP socket connections
- Unix domain sockets
- Any stream-based I/O

### In-Process Channel

For components in the same process, `Channel::duplex()` creates paired
endpoints and skips serialization entirely. Relays forward each received
`TransportFrame` without unpacking it; this preserves the original
representation of malformed wire input.

Benefits:

- **Zero serialization overhead**: Messages passed by value
- **Same-process efficiency**: Ideal for conductor with in-process proxies
- **Explicit wire state**: No serialize/parse round trip is required, while a
  malformed value received from a physical transport remains an explicit frame

### Typed Component Interface

The community [`yosh:acp@8.0.1`](https://wasm.directory/yosh/acp/8.0.1) package
defines an independently versioned typed WIT projection of ACP for the WASI
0.3 async component model. In that design, the host processes the ACP JSON-RPC
connection outside the component and maps it to typed calls across the
component boundary. The package provides interface definitions, not a
transport or integration supplied by this SDK.

## Use Cases

### 1. Standard Agent (Stdio)

Use native `Stdio` for the current process or `AcpAgent` for a child process.
Both use the same frame-aware line transport underneath.

`AcpAgent` shuts its child down as the ACP stdio transport specifies. Its driver
supports graceful finish, so when a `connect_with` foreground returns (or the
agent closes its stdout under `connect_to`), the transport drains accepted
output and then closes the agent's stdin. The agent gets a one-second grace
period to exit on its own; a nonzero exit in that window is reported as an
error, with a bounded stderr tail. If the agent is still running, then on Unix
its process group (which includes agents behind `npx` or `uvx` launchers) is
sent `SIGTERM`, followed by `SIGKILL` if it has not exited two seconds later.
On Windows the process is killed. An exit that the SDK forced this way is not
reported as an error.

Dropping the connection future is cancellation and cannot wait, so it kills
the agent's process group immediately with `SIGKILL`.

### 2. In-Process Proxy Chain

Connect builders, proxies, and conductor components directly. Their default
`ConnectTo` adapter uses `Channel`, so complete frames cross each wrapper with
no serialization.

### 3. Network-Based Components

Split the socket and pass compatible read/write halves to `ByteStreams::new`.

### 4. WebAssembly Embedding

Embedders supply and drive their own runtime and host transport:

- Exchange `TransportFrame` values through an in-component `Channel`. A caller
  using `ConnectTo::into_channel_and_future` polls a present owned driver
  alongside traffic. When no driver is returned, preserve the channel halves'
  independent lifetimes; absence is not EOF.
- Exchange newline-delimited JSON through `Lines`, using a
  `futures::Sink<String>` and
  `futures::Stream<Item = std::io::Result<String>>`.
- Use `ByteStreams` with `futures::io::AsyncRead` and `AsyncWrite`.

### 5. Testing with Mock Transport

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
