# Runnable Protocol V2 Quickstart

The core crate includes a small ACP v2 agent and client that run together over
stdio. Both are compiled examples behind the `unstable_protocol_v2` feature:

- [`simple_agent_v2.rs`](https://github.com/agentclientprotocol/rust-sdk/blob/main/src/agent-client-protocol/examples/simple_agent_v2.rs)
  implements initialization and the complete baseline session lifecycle.
- [`v2_one_shot_client.rs`](https://github.com/agentclientprotocol/rust-sdk/blob/main/src/agent-client-protocol/examples/v2_one_shot_client.rs)
  initializes the agent, creates a session, sends one prompt, renders text
  output, waits for the matching session to become idle, and closes it.

## Run the pair

Build both examples from the repository root:

```bash
cargo build -p agent-client-protocol \
  --features unstable_protocol_v2 \
  --examples
```

Then point the client at the agent executable:

```bash
./target/debug/examples/v2_one_shot_client \
  --command ./target/debug/examples/simple_agent_v2 \
  "Hello from ACP v2"
```

The result shows the two independent parts of a v2 prompt:

```text
Prompt accepted; waiting for session output and completion...
Echo: Hello from ACP v2
Session is idle: Some(EndTurn)
```

The agent writes only JSON-RPC to stdout because ACP uses stdout as the wire.
Write logs and diagnostics to stderr when extending it.

## Client lifecycle

The client installs its `session/update` and `session/request_permission`
handlers before it opens a session. Permission requests are part of the
baseline client surface and have no capability marker; this non-interactive
example cancels them explicitly. It then follows this sequence:

1. Send `initialize` and verify that the agent advertised session support.
2. Send `session/new` and retain the returned command handle and session ID.
3. Send `session/prompt` and await its response. This only confirms acceptance.
4. Ignore queued updates for that new session until its foreground state becomes
   `running`. The running update may already be queued when prompt acceptance
   arrives.
5. Project subsequent message updates, and treat the next matching
   `state_update` with `idle` as completion of foreground work. An idle update
   queued before running is only the session's earlier ready state. Background
   updates may still arrive afterward.
6. Send `session/close` when the client no longer needs the active session.

Real clients normally maintain one shared update projection for every session.
Do not install a temporary handler after sending a prompt: updates can arrive
before the prompt response and are not scoped to a prompt or turn ID.
Within that projection, message chunks append by `messageId`; a later message
snapshot with concrete content replaces the accumulated chunks, `null` clears
them, and omitted content preserves them. Rendering chunks and then rendering a
snapshot again would duplicate output.

## Agent lifecycle

Advertising `AgentCapabilities::session(SessionCapabilities::new())` commits an
agent to the baseline session surface. The example handles:

- `session/new`
- `session/list`
- `session/resume`
- `session/close`
- `session/prompt`
- `session/cancel`
- `session/update` notifications sent to the client

The prompt handler validates and marks the session busy, responds to
`session/prompt` immediately, and moves the actual work into a spawned task so
the connection can continue dispatching cancellation and other traffic. That
task sends the accepted user message, a running update, output, and finally an
idle update with a stop reason.

The example keeps history in memory and supports replay from the start before
the `session/resume` response. A production agent should replace this with
durable session storage, define its supported replay cursors, and make resource
cleanup and cancellation robust across process failure.

For the connection APIs, proxy routing, and compatibility details surrounding
these examples, continue with [Protocol V2](./protocol-v2.md).
