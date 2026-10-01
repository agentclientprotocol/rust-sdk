# Migrating Connection Drivers

`ConnectTo::into_channel_and_future` now returns `(Channel, ConnectionDriver)`
instead of `(Channel, BoxFuture<'static, Result<()>>)`. The same change applies
when accessing a component through `DynConnectTo`.

This is a source-breaking transport-adapter change. It does not change ACP wire
messages, the raw `Channel` sender/receiver types, or `unbounded_send`, and it
introduces no new frame-size, queue, or task limits.

## Components using the default conversion

If your component implements only `connect_to`, no change is needed. The
default conversion still creates a channel pair and drives your component, now
returning an owned `ConnectionDriver`.

Existing callers that infer the returned type and await the driver continue
to work:

```rust,ignore
let (channel, driver) = component.into_channel_and_future();
// Use channel while continuing to poll the driver.
driver.await?;
```

## Custom conversion overrides

Import `ConnectionDriver` from `agent_client_protocol` and change the return
type. Wrap a future that owns the connection work with `ConnectionDriver::new`:

```rust,ignore
fn into_channel_and_future(self) -> (Channel, ConnectionDriver) {
    let (channel, future) = self.into_channel_transport();
    (channel, ConnectionDriver::new(future))
}
```

For an endpoint whose work is driven elsewhere, return
`ConnectionDriver::passive()` instead of wrapping a ready no-op future:

```rust,ignore
fn into_channel_and_future(self) -> (Channel, ConnectionDriver) {
    (self.channel, ConnectionDriver::passive())
}
```

Passive drivers are awaitable and immediately succeed, but that success is
**not EOF**. Bridges must check `is_passive()` before using driver completion
as a lifetime signal. Passive bridges retain each read/write half until its
own closure; an input half-close can still be followed by a final response.

If a wrapper simply exposes another component's endpoint, return its original
`(channel, driver)` pair. Re-boxing that driver and wrapping it with `new`
would erase the passive distinction and any built-in finish coordination.

## Completion and drain responsibilities

Poll owned work and outbound forwarding concurrently. A driver may need its
outbound request to be delivered before it can receive a response and finish.

An adapter must not report success before flushing output it already accepted.
Custom normalized drivers should finish their own work and drain output after
their channel input closes. The SDK cannot infer how to flush an arbitrary
opaque future or external buffer.

Built-in `Lines` and `ByteStreams` preserve normal half-close behavior. When
their owner explicitly finishes, they drain accepted output while continuing
to poll incoming I/O for errors, rather than waiting for unrelated remote
input to reach EOF. Errors may terminate the connection without graceful drain.

See [Transport Architecture](./transport-architecture.md#component-boundary)
for the active/passive boundary and forwarding rules.
