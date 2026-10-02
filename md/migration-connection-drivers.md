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
returning `Some(ConnectionDriver)`. That default wraps opaque work; it cannot
infer a physical finish hook. A buffered transport that needs a finite
foreground to await physical flush should override normalization with
`with_finish`, as described below.

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
real owned work; cooperative drivers additionally support a finish hook.
Passive bridges retain each read/write half until its
own closure; an input half-close can still be followed by a final response.

If a wrapper simply exposes another component's endpoint, return its original
`(channel, optional_driver)` pair. Re-boxing an owned driver and wrapping it
with `new` would hide its finish capability. Inventing a ready
driver for `None` would also turn absence into a false completion signal.

For tracing, error annotation, or completion cleanup, decorate the future with
`map_future`. This preserves both the finish capability and any already-issued
request; opaque work stays opaque:

```rust,ignore
use futures::FutureExt;

let (channel, driver) = component.into_channel_and_future();
let driver = driver.map(|driver| {
    driver.map_future(|work| {
        work.inspect(|result| eprintln!("transport completed: {result:?}"))
    })
});
(channel, driver)
```

The transformed future must still drive the original work and must not report
success before its accepted output has drained.

## Completion and drain responsibilities

Poll owned work and outbound forwarding concurrently. A driver may need its
outbound request to be delivered before it can receive a response and finish.

An adapter must not report success before flushing output it already accepted.
Use `ConnectionDriver::with_finish(future, finish)` for a custom normalized
transport that needs to flush during finite foreground shutdown. The
nonblocking `FnOnce()` hook requests graceful completion; the future proves
completion and reports any I/O error.

```rust,ignore
let (finish_tx, finish_rx) = futures::channel::oneshot::channel();
let future = async move {
    // Keep processing input and output while waiting for the finish request.
    // A dropped sender is not a finish request; it may simply mean that
    // finish control was abandoned while normal half-closes remain in use.
    //
    // After a successful signal, seal the outgoing queue, drain every accepted
    // frame, and flush/close the physical write half. Do not wait for remote
    // read EOF; continue observing genuine I/O errors during the drain.
    run_custom_adapter(outgoing_rx, physical_io, finish_rx).await
};
let driver = ConnectionDriver::with_finish(future, move || {
    let _ = finish_tx.send(());
});
(channel, Some(driver))
```

SDK shutdown coordination invokes this hook only after protocol output has
been handed off to the normalized transport, then awaits the driver. Low-level
callers can use `driver.request_finish()` themselves. A `true` return means
cooperative finish is supported and has been requested, including a request
already issued. Requests are idempotent, but the hook runs only once. A `false`
return means opaque work, not "already requested."

Requesting finish does not prove output has finished flushing; continue polling
or await the driver. Capability remains intact if that driver is handed to
another owner while flushing. Quiesce and hand off output before requesting
finish; idempotence does not permit new output after sealing. Dropping the
driver drops its owned future without a graceful request. Dropping only the
hook does not invoke it or necessarily stop that future.

There is no implicit timeout. A cooperative adapter that cannot flush keeps
the connection pending, so applications that need a deadline must impose one
and accept that cancelling it can truncate output. `with_finish` declares the
adapter's contract; it cannot make an arbitrary future or external buffer
flush automatically.

Built-in `Lines` and `ByteStreams` preserve normal half-close behavior. When
their owner explicitly finishes, they drain accepted output while continuing
to poll incoming I/O for errors, rather than waiting for unrelated remote
input to reach EOF. Errors may terminate the connection without graceful drain.

## Direct adapter entry point

Returning a cooperative driver from `into_channel_and_future` lets normalized
SDK consumers coordinate finish. A custom transport's direct `connect_to`
implementation must coordinate it too: `try_join!(bridge, driver)` alone can
wait forever after a finite peer has returned.

This scaffold follows the built-in `Lines` policy, using only public APIs:

```rust,ignore
use agent_client_protocol::{Channel, ConnectTo, ConnectionDriver, Result, UntypedRole};
use futures::{future::{select, Either}, FutureExt};

struct BufferedAdapter {
    channel: Channel,
    driver: ConnectionDriver,
}

impl ConnectTo<UntypedRole> for BufferedAdapter {
    async fn connect_to(self, peer: impl ConnectTo<UntypedRole>) -> Result<()> {
        let bridge = Box::pin(self.channel.connect_to(peer));
        match select(bridge, self.driver).await {
            Either::Left((result, mut driver)) => {
                result?; // The peer's accepted output has been handed off.
                if driver.request_finish() {
                    driver.await // Prove physical drain; propagate its errors.
                } else {
                    // Preserve a ready error, then cancel opaque work.
                    driver.now_or_never().unwrap_or(Ok(()))
                }
            }
            Either::Right((result, _bridge)) => result,
        }
    }

    fn into_channel_and_future(self) -> (Channel, Option<ConnectionDriver>) {
        (self.channel, Some(self.driver))
    }
}
```

The adapter's own future must own/close its physical producers before reporting
completion. Its finish implementation must stop forwarding successful input
to a completed peer while still observing genuine read errors during drain.
Otherwise, late input can fail against the dropped receiver and cancel final
output. Both direct and normalized entry points should be tested with output
backpressure and independently open input.

## Finite foreground shutdown

On successful `Builder::connect_with` foreground completion, routable queued
output is drained. Requests still blocked on unresolved readiness are failed
and removed rather than published after shutdown. Physical transport and
protocol progress continue during this drain; queued application tasks are
not started merely to finish the sink. The inherited cleanup coordinator can
still poll application tasks while protecting a close callback already
underway; a blocked close callback can delay completion.

Foreground success stops beginning new application delivery or close
callbacks. Physical reads remain driven without delivering their input to the
completed foreground. A close callback already underway finishes before the
outgoing drain boundary seals, and its errors retain precedence.

Protocol connectors and routers use the same ownership-aware rule. An owned
foreground's completion requests cooperative drain instead of waiting for
unrelated remote input. Initialization rejection also hands off its reply
before requesting finish. Passive half-closes alone do not request finish;
they preserve the other direction for a final response.

Cooperative drivers, both built-in and custom, are awaited through physical
write shutdown.
An opaque driver constructed with `ConnectionDriver::new(future)` has no
externally requestable finish control: finite foreground shutdown transfers
protocol output into its normalized channel, then cancels that work without
guaranteeing custom physical flush. Reactive `connect_to` still joins owned
work after input EOF. Choose `new` for opaque cancellable work and `with_finish`
when the adapter can honor an explicit graceful-finish request.

See [Transport Architecture](./transport-architecture.md#component-boundary)
for the active/passive boundary and forwarding rules.
