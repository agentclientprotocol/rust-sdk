# Request Cancellation

The SDK exposes the ACP `$/cancel_request` notification behind the
`unstable_cancel_request` feature. The notification is protocol-level: either
side may send it to ask the peer to cancel one outstanding JSON-RPC request by
ID.

Enable the feature when depending on the crate:

```toml
agent-client-protocol = { version = "...", features = ["unstable_cancel_request"] }
```

Cancellation is cooperative. A peer may ignore `$/cancel_request`, may finish
with normal data, or may respond to the original request with
`Error::request_cancelled()` (`-32800`). The requesting side always receives a
response to the original request; cancellation only changes _which_ response
that is. The SDK ignores unhandled `$/...` notifications (even when the
feature is disabled) so unsupported protocol-level notifications do not
produce method-not-found errors.

## Cancelling outgoing requests

To cancel a request sent through `ConnectionTo::send_request`, keep the
returned `SentRequest` and call `cancel` on it:

```rust
# use agent_client_protocol::{ConnectionTo, Error, UntypedRole};
# use agent_client_protocol_test::MyRequest;
# async fn example(cx: ConnectionTo<UntypedRole>) -> Result<(), Error> {
let request = cx.send_request(MyRequest {});
request.cancel()?;

// The peer still responds to the request: with normal data if it raced
// ahead, or with the standard cancellation error.
let result = request.block_task().await;
# let _ = result;
# Ok(())
# }
```

The `SentRequest` remembers the peer and any proxy wrapping used for the
original request, so this also works for requests sent through
`ConnectionTo::send_request_to`.

Dropping a `SentRequest` before the SDK receives a response also sends
`$/cancel_request`. This covers abandoned request handles and futures. Once the
SDK routes a response to the waiting request handle, automatic cancellation is
disarmed, even if caller code has not yet consumed it with `block_task`,
`on_receiving_result`, or `forward_response_to`.

If you already have the JSON-RPC request ID, send the notification directly:

```rust
# use agent_client_protocol::{ConnectionTo, Error, UntypedRole};
# async fn example(cx: ConnectionTo<UntypedRole>) -> Result<(), Error> {
cx.send_cancel_request("request-id".to_string())?;
# Ok(())
# }
```

## Handling cancellation of incoming requests

For incoming requests, get the request-local cancellation marker from the
`Responder`. This keeps cancellation handling next to the request work it
controls:

```rust
# use agent_client_protocol::{ConnectionTo, Error, Responder, UntypedRole};
# use agent_client_protocol_test::{MyRequest, MyResponse};
# async fn example(request: MyRequest, responder: Responder<MyResponse>, cx: ConnectionTo<UntypedRole>) -> Result<(), Error> {
# async fn run_request(_request: MyRequest) -> Result<MyResponse, Error> { todo!() }
let cancellation = responder.cancellation();

cx.spawn(async move {
    let response = cancellation.run_until_cancelled(run_request(request)).await;
    responder.respond_with_result(response)
})?;
# Ok(())
# }
```

`run_until_cancelled` is the simple path for handlers that should stop work and
reply with the standard cancellation error as soon as cancellation is
requested; it drops the work future when cancellation wins, so cleanup must
happen in `Drop` implementations and partial results are lost. If the handler
needs cleanup, partial results, or custom cancellation behavior, use
`cancellation.cancelled()` or `cancellation.is_cancelled()` directly inside
the request work instead.

Cancellation markers are only updated when the connection can process the
incoming `$/cancel_request` notification. Long-running handlers should return
quickly and move work into `ConnectionTo::spawn`, `SentRequest` callbacks, or
another task.

## Proxies

When proxying with `SentRequest::forward_response_to`, the SDK observes the
upstream `Responder` cancellation marker and forwards cancellation to the
downstream request automatically. The downstream response (normal data or a
cancellation error) is still forwarded back upstream.

## Low-level access

Register `CancelRequestNotification` or `ProtocolLevelNotification` directly
only when you need low-level access to cancellation notifications, such as
custom routing or protocol tracing:

```rust
# use agent_client_protocol::{ConnectionTo, Error, UntypedRole};
use agent_client_protocol::schema::CancelRequestNotification;

# fn example() {
let builder = UntypedRole.builder().on_receive_notification(
    async |cancel: CancelRequestNotification, _cx: ConnectionTo<UntypedRole>| {
        // Mark the matching in-flight operation cancelled.
        let _request_id = cancel.request_id;
        Ok(())
    },
    agent_client_protocol::on_receive_notification!(),
);
# let _ = builder;
# }
```
