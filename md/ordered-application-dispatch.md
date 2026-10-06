# Ordered Application Dispatch

The SDK's inbound ordering does not automatically extend to an application's UI
executor. A notification handler may have finished enqueueing an update while
the application has not yet applied it. Awaiting a request with `block_task()`
on another task does not drain that application queue.

Use **one FIFO queue with one sequential consumer** for:

1. Notifications, enqueued by typed connection handlers.
2. Response results, enqueued by `SentRequest::on_receiving_result`.
3. Connection closure, enqueued by `Builder::on_close`.

Apply updates in the consumer and expose a response result only when the
consumer reaches that event. For v2 resume, this makes preceding replay visible
to application code before it observes the resume result. Do not spawn a
separate application task for each event unless that executor also preserves
their application order.

The compiled [ordered application-dispatch cookbook recipe](https://docs.rs/agent-client-protocol-cookbook/latest/agent_client_protocol_cookbook/ordered_application_dispatch/)
demonstrates this with an application callback that need not be `Send`.
Connection handlers retain only event senders, not the application view or an
owning connection. This is an integration pattern, not a new session state
machine or coordinator.

## Response barriers are short dispatch steps

Register `on_receiving_result` immediately when sending the request, before
yielding or transferring the request to another task. If registered before the
response is routed in its original dispatch, the callback holds dispatch until
it returns. The barrier is not retroactive: already-routed responses and later
routing through a retained `ResponseRouter` do not hold subsequent wire traffic.

The callback should enqueue the result and return. **Do not await another
inbound response or notification on that connection inside the callback.**
That traffic cannot be dispatched until the callback finishes, causing a
deadlock even on a multithreaded runtime. Perform work that needs later traffic
in the application consumer, the `connect_with` foreground, or a task started
with `ConnectionTo::spawn`.

If another application task needs a projection-drained acknowledgement, the
consumer can send it after applying the response marker. The SDK callback
should not wait for that acknowledgement.

See the [SDK ordering contract](https://docs.rs/agent-client-protocol/latest/agent_client_protocol/concepts/ordering/)
for the full callback and response-routing semantics. Queue capacity is an
application decision: the recipe uses an unbounded queue, while a bounded
queue requires a backpressure policy that cannot deadlock inbound dispatch.

## Closure is an application event too

Clean incoming EOF follows already-received notifications and timely ordered
wire responses. Putting `on_close` on the same queue lets the consumer apply
those events before marking the connection closed.

EOF fails pending SDK requests before running close callbacks, but their
synthetic error callbacks are not ordered wire-response barriers and can
enqueue results after the closure event. The application should settle its
outstanding operations at `Closed` and tolerate late completions, rather than
waiting for every callback before accepting closure. Transport and handler
errors still propagate from the connection future; this pattern does not turn
all failures into clean EOF.

## Do not add prompt attribution

The queue preserves dispatch order, not a relationship the protocol does not
provide. A v2 prompt response acknowledges acceptance. Session `running` and
`idle` updates describe foreground state, not a specific prompt's completion,
and background updates may continue while idle. Keep these as separate
application events; do not assign the next `Idle` to a prompt without an
additional protocol guarantee. See [high-level v2 sessions](./protocol-v2.md#high-level-v2-sessions).
