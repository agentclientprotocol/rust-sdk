# Request Cancellation

This chapter documents the `$/cancel_request` protocol-level notification and
how the SDK implements it.

For API usage (cancelling a `SentRequest`, observing cancellation from a
`Responder`), see the `concepts::cancellation` chapter in the
[agent-client-protocol rustdoc](https://docs.rs/agent-client-protocol). The
SDK support is gated behind the `unstable_cancel_request` feature:

```toml
agent-client-protocol = { version = "...", features = ["unstable_cancel_request"] }
```

## The `$/cancel_request` Notification

Either side of a connection may send `$/cancel_request` to ask the peer to
cancel one outstanding JSON-RPC request, identified by its ID:

```json
{
  "jsonrpc": "2.0",
  "method": "$/cancel_request",
  "params": {
    "requestId": "70b9f1c9-c2a3-4bd2-b6b9-65a06d96b675"
  }
}
```

`requestId` is the JSON-RPC `id` of the request to cancel, as allocated by the
sender of that request (a string, number, or null).

## Semantics

Cancellation is **cooperative**. After receiving `$/cancel_request`, the peer
may:

- ignore it and respond to the request normally,
- finish early with whatever data it has, or
- respond to the original request with the standard cancellation error,
  code `-32800` ("Request cancelled").

The requesting side always receives a response to the original request;
cancellation only changes _which_ response that is. A `$/cancel_request` for
an unknown or already-completed request ID is silently ignored.

## Interoperability

Protocol-level (`$/`-prefixed) notifications are optional by design. The SDK
ignores unhandled `$/` notifications instead of rejecting them with a
method-not-found error, and does so even when the `unstable_cancel_request`
feature is disabled. A peer that sends `$/cancel_request` to a component built
without cancellation support therefore loses nothing: the request simply runs
to completion.

## Proxy Chains

Cancellation propagates **hop by hop** rather than end to end. Request IDs are
allocated per connection, so a `$/cancel_request` only ever refers to a
request on the connection it is sent over:

1. The client sends `$/cancel_request` for a request it made to its direct
   peer (for example, a proxy).
2. A proxy that forwarded the request downstream (the SDK does this with
   `forward_response_to`) reacts by sending its own `$/cancel_request` for the
   downstream request, using the downstream connection's request ID.
3. The downstream response — normal data or the cancellation error — flows
   back up the chain as the response to each hop's request.

When the notification targets a request that was wrapped in a
`_proxy/successor` envelope (see the [Protocol Reference](./protocol.md)), the
`$/cancel_request` is wrapped in the same envelope, and `requestId` refers to
the JSON-RPC `id` of the wrapped request on that connection.

## Related Documentation

- [Protocol Reference](./protocol.md) - The `_proxy/successor/*` envelope protocol
- [agent-client-protocol rustdoc](https://docs.rs/agent-client-protocol) - SDK API for sending, observing, and forwarding cancellations (see `concepts::cancellation`)
