# Session Operation Coordination

The private
[`v2_session_coordination`](https://github.com/agentclientprotocol/rust-sdk/blob/main/src/agent-client-protocol/examples/v2_session_coordination.rs)
example is a **cookbook prototype**, not a public SDK coordinator. It shows one
application policy for sharing draft-v2 `session/resume` replay and sequencing
`session/close` on a single application executor. Its state uses `Rc`, so it is
not a cross-thread service.

## Ownership and ordering

The example initializes the connection before creating application owners.
`Sessions`, pending loads, and delivered sessions are the only owners of the
command sender. SDK callbacks own a separate sender for ordered wire events;
they never capture an application owner or a connection. The caller owns and
may cancel the enclosing connection future, including during initialization.

One driver consumes notifications, resume and close response markers, and EOF
from a single wire-event FIFO. This extends the
[ordered application dispatch](./ordered-application-dispatch.md) pattern:
callbacks only enqueue events and never await another inbound response. The
driver installs cleanup state before it considers reopening a session.
Already-queued application commands take priority over wire events, so release
and acquisition decisions are honored before exposing a response.
Synthetic failure callbacks for unanswered requests can arrive after closure;
the application must not wait for them. When the last command sender is
dropped, the foreground transport stops without waiting for unanswered wire
operations. Keep an application owner alive to continue cleanup; the driver
does not keep the transport alive solely to obtain a close acknowledgement.

## Load policy

Every load uses the fixed working directory passed to `with_sessions` and
`ReplayFrom::Start`.

- Concurrent active loads for one session share one resume operation and one
  projection. Dropping only some loads does not affect those that remain.
- A lease covers both consumed sessions and results that were delivered but
  never consumed, so either path eventually releases its load.
- If every load is abandoned during resume, accumulated replay is discarded,
  while lightweight state remains to settle the in-flight wire response.
- A replacement arriving during abandonment or close waits for the old resume
  response and a successful close, then starts a fresh replay. Cleanup state is
  recorded before reopening.
- Resume failure is returned to that operation's loaders; a waiting or later
  load may retry.
- Close failure fails queued replacements and blocks only that session until
  reconnect. There is no automatic close retry.

The selected policy is **drain the published resume, then close**. It does not
cancel or detach the resume.

## Projection and completion

The projection is intentionally just a raw, lossless `SessionUpdate` log, not a
general reducer. The complete resume setup response is retained alongside that
log by the operation result. Loads expose their shared projection only after
the resume response; live updates continue appending afterward. This example
does not expose partial replay to a loading view. Production applications
should project protocol entities according to their own UI needs, outside SDK
callbacks.

Do not generalize the next `idle` state into completion of a particular prompt:
v2 state is session-wide, updates have no prompt identity, and background
updates may continue while idle.

## Limitations

This prototype has no timeouts, automatic retries, MCP attachments, session
new/fork, or prompt scheduling. It deliberately uses unbounded command/event
queues and an unbounded update log for clarity; production code needs explicit
memory, backpressure, and fairness policies. UI work and user code must not run
in SDK callbacks.

Run it against an agent with an existing resumable session:

```sh
cargo run -p agent-client-protocol --features unstable_protocol_v2 \
  --example v2_session_coordination -- \
  --command 'my-agent-command' --session-id 'existing-session-id'
```

The repository verification command is:

```sh
just test
```

The prototype's focused tests exercise abandonment, replacement, failure, and
shutdown behavior; this page does not duplicate the implementation.
