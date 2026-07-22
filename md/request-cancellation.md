# Request Cancellation

This chapter documents the `$/cancel_request` protocol-level notification and
how the SDK implements it.

For API usage (cancelling a `SentRequest`, observing cancellation from a
`Responder`), see the `concepts::cancellation` chapter in the
[agent-client-protocol rustdoc](https://docs.rs/agent-client-protocol).

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
an unknown or already-completed request ID is silently ignored. A
`$/cancel_request` with malformed params (for example, a `requestId` that is
not a string, number, or null) is logged and ignored without a reply, like any
other malformed notification.

## Interoperability

Protocol-level (`$/`-prefixed) notifications are optional by design. The SDK
ignores unhandled notifications instead of rejecting them with a
method-not-found error. A peer that sends `$/cancel_request` to a component
that does not support cancellation therefore loses nothing: the request simply
runs to completion.

Dropping an unconsumed `SentRequest` asks the peer to cancel it. Use
`SentRequest::detach()` for requests whose eventual response should be ignored,
but which should continue running on the peer. The peer is still expected to
answer the JSON-RPC request eventually; use a notification instead when no
response is expected at all.

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

Because the notification is hop-scoped, it is never tunneled across hops:
generic forwarding helpers (`send_proxied_message_to` in the SDK, and the
conductor's internal routing) drop a raw `$/cancel_request` instead of
forwarding a request ID that means nothing on the next connection. The
cancellation still reaches the next hop, re-issued by `forward_response_to`
with that hop's own request ID.

Proxies that intercept methods with custom handlers stay in control: the
request's cancellation marker is their decision point, and handlers see the
raw notification before any generic forwarding fallback. A custom handler can
handle the cancellation locally, propagate it to a forwarded request
(`forward_response_to`, or `forward_cancellation_from` when the forwarding
needs custom logic), absorb it, or claim the notification and route it itself.
See the `concepts::cancellation` chapter in the
[agent-client-protocol rustdoc](https://docs.rs/agent-client-protocol) for
the full decision matrix.

When the notification targets a request that was wrapped in a
`_proxy/successor` envelope (see the [Protocol Reference](./protocol.md)), the
`$/cancel_request` is wrapped in the same envelope, and `requestId` refers to
the JSON-RPC `id` of the wrapped request on that connection.

The conductor translates cancellations between hops. Since request IDs are
reallocated at every hop, a raw `$/cancel_request` cannot match anything
beyond the hop it was sent over; the conductor re-issues cancellation with the
next hop's request ID instead.

## Related Documentation

- [Protocol Reference](./protocol.md) - The `_proxy/successor` envelope protocol
- [agent-client-protocol rustdoc](https://docs.rs/agent-client-protocol) - SDK API for sending, observing, and forwarding cancellations (see `concepts::cancellation`)
