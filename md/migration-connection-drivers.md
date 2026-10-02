# Migrating Connection Drivers

`ConnectTo::into_channel_and_future` now returns
`(Channel, Option<ConnectionDriver>)` instead of
`(Channel, BoxFuture<'static, Result<()>>)`. The same change applies when
accessing a component through `DynConnectTo`.

This is a source-breaking transport-adapter change. It does not change ACP wire
messages, the raw `Channel` sender/receiver types, or `unbounded_send`, and it
introduces no new frame-size, queue, or task limits.

## Components using the default conversion

If your component implements only `connect_to`, no change is needed. The
default conversion still creates a channel pair and drives your component, now
returning `Some(ConnectionDriver)`.

Low-level callers must handle the optional work explicitly. The optional value
is not a future: awaiting it directly no longer compiles. For a component that
is known to own work, extract its driver before polling it:

```rust,ignore
let (channel, driver) = component.into_channel_and_future();
let driver = driver.expect("this component owns connection work");
// Use channel while continuing to poll the driver.
driver.await?;
```

For a generic component, handle both cases: poll `Some(driver)` alongside
traffic and drain accepted output on completion; for `None`, retain the
channel's independent halves until they close. Absence of work is not EOF.
Do not replace `None` with a ready-success future in a shutdown race.

## Custom conversion overrides

Import `ConnectionDriver` from `agent_client_protocol` and change the return
type. Wrap a future that owns the connection work with `ConnectionDriver::new`:

```rust,ignore
fn into_channel_and_future(self) -> (Channel, Option<ConnectionDriver>) {
    let (channel, future) = self.into_channel_transport();
    (channel, Some(ConnectionDriver::new(future)))
}
```

For an endpoint whose work is driven elsewhere, return `None` instead of
wrapping a ready no-op future:

```rust,ignore
fn into_channel_and_future(self) -> (Channel, Option<ConnectionDriver>) {
    (self.channel, None)
}
```

An existing `Channel` has no driver. There is no awaitable passive sentinel,
and no finish hook belongs to the `None` case. `ConnectionDriver` always holds
real owned work; some built-in owned drivers additionally support private
finish coordination. Passive bridges retain each read/write half until its
own closure; an input half-close can still be followed by a final response.

If a wrapper simply exposes another component's endpoint, return its original
`(channel, optional_driver)` pair. Re-boxing an owned driver and wrapping it
with `new` would erase its built-in finish coordination. Inventing a ready
driver for `None` would also turn absence into a false completion signal.

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
