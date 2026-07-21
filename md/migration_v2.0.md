# Migrating from `agent-client-protocol` 1.x to 2.0

Version 2.0 changes the low-level in-process transport boundary so JSON-RPC frames remain intact
across components and adapters.

## Raw channels carry frames

`Channel` is now the single batch-aware in-process transport boundary. Its `rx` and `tx` carry
`TransportFrame`, not `Result<RawJsonRpcMessage, Error>`:

```rust
use agent_client_protocol::{Channel, RawJsonRpcMessage, TransportFrame};

# fn send(channel: &Channel, message: RawJsonRpcMessage) {
channel.tx.unbounded_send(TransportFrame::Single(message)).unwrap();
# }
```

`TransportFrame` distinguishes a valid single message, a non-empty `TransportBatch`, and an
explicit malformed wire value. Transport I/O failures are returned by the future from
`ConnectTo::into_channel_and_future`; they are never channel items. Components should preserve a
received frame intact when relaying it so batch response grouping remains correct.

The separate, hidden `FramedChannel` and `into_framed_channel_and_future` compatibility path no
longer exist. Implement only `ConnectTo::connect_to`, optionally overriding
`into_channel_and_future` for a direct channel adapter.
