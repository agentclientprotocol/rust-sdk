# Sending Requests

Both `ConnectionTo` and `V2ConnectionTo` support two request APIs:

- **`send_request(request)`** sends immediately and returns a `SentRequest`.
  Keep this when eager publication is useful, such as starting work before
  choosing how to consume its result. Existing behavior is unchanged.
- **`prepare_request(request)`** returns a `PreparedRequest` without sending.
  Choose this when response handling must be selected before the peer can reply,
  especially for ordered callbacks from outside the connection future.

Both have an explicit-peer variant: `send_request_to(peer, request)` and
`prepare_request_to(peer, request)`.

Publication means synchronously registering the pending reply and enqueueing
the request. Peer transformation and physical transmission happen later in the
connection driver; successful publication does not acknowledge peer receipt.

## Select handling before publication

```rust
connection.prepare_request(request).on_receiving_result(async move |result| {
    application_queue.enqueue(result)?;
    Ok(())
})?;
```

The callback task and ordering marker are installed before the request enters
the outgoing queue. When the peer response is routed during its original
dispatch, the loop waits for the callback to finish before dispatching later
messages. Success and error responses have the same ordering.

`on_receiving_ok_result(responder, callback)` and
`forward_response_to(responder)` also select ordered handling before sending.
`map(...)` and `forward_cancellation_from(...)` configure a prepared request
without publishing it.

Keep ordered callbacks short. They must not await another response,
notification, or other inbound traffic on the same connection: dispatch cannot
deliver that traffic until the callback returns. Enqueue application work and
return, or spawn follow-up work.

Eager `send_request(...).on_receiving_result(...)` retains its existing
conditional guarantee: if response routing wins the race with registration,
the callback runs without a barrier. Even immediate chaining can race when the
connection runs concurrently. Neither API imposes a barrier on EOF-generated
failures or on a retained `ResponseRouter` routed after its original dispatch.

## Unordered consumption

`prepare_request(...).block_task()` sends **during the method call**, then
returns a response future. It does not wait for the first poll to send:

```rust
let response = connection.prepare_request(request).block_task();
connection.send_notification(notification)?; // Queued after the request.
let result = response.await?;
```

Await that future only outside incoming handlers. Dispatch does not wait for
the caller to process its result.

`prepare_request(...).detach()?` sends and discards the eventual response
without cancelling. It does not select ordered consumption.

Outgoing order follows publication, not preparation. A notification sent
between preparation and consumption enters the queue before the request.

## Cancellation after selecting a consumer

Obtain `cancellation_handle()` before consuming a prepared or sent request to
retain explicit cancellation control alongside its callback or response future:

```rust
let prepared = connection.prepare_request(request);
let cancellation = prepared.cancellation_handle();
prepared.on_receiving_result(async move |result| {
    application_queue.enqueue(result)?;
    Ok(())
})?;
cancellation.cancel()?;
```

Cancelling does not discard the eventual result. The cloneable handle shares
SDK settlement and once-only cancellation state, rather than sending an
unconditional notification for a saved request ID. Dropping the handle does
nothing. Calls before publication, after a response or local failure, or after
detaching the request do nothing. See [Request Cancellation](./request-cancellation.md)
for the full contract, including publication races and peer wrapping.

## Drop and errors

Dropping an unconsumed `PreparedRequest` sends nothing. Dropping the response
future returned by `block_task()` asks the peer to cancel an outstanding request,
as dropping an eager `SentRequest` does.

Preparation and publication failures reach the selected callback or response
future. Callback registration failure returns an error without sending the
request. `detach()` returns immediate preparation and enqueue failures directly;
later local transformation errors and peer response errors are discarded.
Transport failures still propagate through the connection future. Returning an
error from a callback terminates the connection, so handle expected request
errors inside it.

See [Ordered Application Dispatch](./ordered-application-dispatch.md) for
extending these guarantees to a separate application executor.
