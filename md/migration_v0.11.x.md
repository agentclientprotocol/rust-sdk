# Migrating from `agent-client-protocol` `0.10.x` to `0.11`

This guide explains how to move existing code to the programming model planned for `agent-client-protocol` `0.11`.

Throughout this guide:

- **old API** = `agent-client-protocol` `0.10.x`
- **new API** = the planned `agent-client-protocol` `0.11` API

All code snippets below use the intended `0.11` import paths.

## 1. Move message types under `schema`

The current `0.10.x` crate exports most protocol message types at the crate root.

The `0.11` API moves most ACP request, response, and notification types under `schema`.

```rust
// Old (0.10.x)
use agent_client_protocol as acp;
use acp::{InitializeRequest, NewSessionRequest, PromptRequest, ProtocolVersion};

// New (0.11)
use agent_client_protocol as acp;
use acp::schema::{
    InitializeRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
};
```

Most ACP request, response, and notification types live under `schema` in `0.11`

## 2. Replace connection construction

The main construction changes are:

- `ClientSideConnection::new(handler, outgoing, incoming, spawn)`
  - becomes `Client.builder().connect_with(ByteStreams::new(outgoing, incoming), async |cx| { ... })`
- `AgentSideConnection::new(handler, outgoing, incoming, spawn)`
  - becomes `Agent.builder().connect_to(ByteStreams::new(outgoing, incoming)).await?`
- custom `spawn` function + `handle_io` future
  - becomes builder-managed connection execution

If you already have stdin/stdout or socket-like byte streams, wrap them with `ByteStreams::new(outgoing, incoming)`.

If you are spawning subprocess agents, prefer `agent-client-protocol-tokio` over hand-rolled process wiring.

If you already have a reason to stay at the raw request/response level, you can still send `PromptRequest` directly with `cx.send_request(...)`; the session helpers are just the default migration path for most client code.

## 3. Replace outbound trait-style calls with `send_request` and `send_notification`

In the old API, the connection itself implemented the remote trait, so calling the other side looked like this:

```rust
conn.initialize(InitializeRequest::new(ProtocolVersion::V1)).await?;
```

In `0.11`, you send a typed request through `ConnectionTo<Peer>`:

```rust
cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
    .block_task()
    .await?;
```

The main replacements are:

| Old style                          | New style                                                    |
| ---------------------------------- | ------------------------------------------------------------ |
| `conn.initialize(req).await?`      | `cx.send_request(req).block_task().await?`                   |
| `conn.new_session(req).await?`     | usually `cx.build_session(...)` or `cx.build_session_cwd()?` |
| `conn.prompt(req).await?`          | usually `session.send_prompt(...)` on an `ActiveSession`     |
| `conn.cancel(notification).await?` | `cx.send_notification(notification)?`                        |

A few behavioral differences matter during migration:

- `send_request(...)` returns a `SentRequest`, not the response directly.
- Call `.block_task().await?` when you want to wait for the response from a
  context that does not block the dispatch loop. The `main_fn` closure passed to
  `connect_with(...)` and tasks spawned via `cx.spawn(...)` are both safe; the
  dispatch loop continues processing messages (including the response you are
  waiting for) in the background.
- Do **not** call `.block_task().await?` inside `on_receive_*` callbacks. Those
  callbacks run on the dispatch loop, so blocking them deadlocks the
  connection. Prefer `on_receiving_result(...)`, `on_receiving_ok_result(...)`,
  `on_session_start(...)`, or `cx.spawn(...)` from handlers.

## 4. Replace manual session management with `SessionBuilder`

One of the biggest user-facing changes is session handling.

Old code typically looked like this:

```rust
let session = conn
    .new_session(NewSessionRequest::new(cwd))
    .await?;

conn.prompt(PromptRequest::new(
    session.session_id.clone(),
    vec!["Hello".into()],
))
.await?;
```

New code usually starts from the connection and uses a session builder:

```rust
cx.build_session_cwd()?
    .block_task()
    .run_until(async |mut session| {
        session.send_prompt("Hello")?;
        let output = session.read_to_string().await?;
        println!("{output}");
        Ok(())
    })
    .await?;
```

Useful replacements:

| Old pattern                                               | New pattern                                                         |
| --------------------------------------------------------- | ------------------------------------------------------------------- |
| `new_session(NewSessionRequest::new(cwd))`                | `build_session(cwd)`                                                |
| `new_session(NewSessionRequest::new(current_dir))`        | `build_session_cwd()?`                                              |
| store `session_id` and pass it into every `PromptRequest` | let `ActiveSession` manage the session lifecycle                    |
| `subscribe()` to observe streamed session output          | `ActiveSession::read_update()` or `ActiveSession::read_to_string()` |
| intercept and rewrite a `session/new` request in a proxy  | `build_session_from(request)`                                       |

Also note:

- use `start_session()` when you want an `ActiveSession<'static, _>` you can keep around
- use `on_session_start(...)` inside `on_receive_*` callbacks when you need to start a session without manually blocking the current task
- use `on_proxy_session_start(...)` or `start_session_proxy(...)` for proxy-style session startup and forwarding

## 5. Replace `Client` trait impls with builder callbacks

In `0.10.x`, your client behavior lived in an `impl acp::Client for T` block.

In `0.11`, register typed handlers on `Client.builder()` instead.

Each `on_receive_*` call takes two arguments: the async handler, and one of
the helper macros `acp::on_receive_request!()`,
`acp::on_receive_notification!()`, or `acp::on_receive_dispatch!()`. These
macros are a temporary workaround until [return-type notation] stabilizes;
pass the one that matches the method you are calling.

[return-type notation]: https://github.com/rust-lang/rust/issues/109417

### Common client-side method mapping

- `request_permission` -> `.on_receive_request(|req: RequestPermissionRequest, responder, cx| ...)`
- `write_text_file` -> `.on_receive_request(|req: WriteTextFileRequest, responder, cx| ...)`
- `read_text_file` -> `.on_receive_request(|req: ReadTextFileRequest, responder, cx| ...)`
- `create_terminal` -> `.on_receive_request(|req: CreateTerminalRequest, responder, cx| ...)`
- `terminal_output` -> `.on_receive_request(|req: TerminalOutputRequest, responder, cx| ...)`
- `release_terminal` -> `.on_receive_request(|req: ReleaseTerminalRequest, responder, cx| ...)`
- `wait_for_terminal_exit` -> `.on_receive_request(|req: WaitForTerminalExitRequest, responder, cx| ...)`
- `kill_terminal` -> `.on_receive_request(|req: KillTerminalRequest, responder, cx| ...)`
- `session_notification` -> `.on_receive_notification(|notif: SessionNotification, cx| ...)`
- `ext_method` / `ext_notification` -> your own derived `JsonRpcRequest` / `JsonRpcNotification` types, or a catch-all `on_receive_dispatch(...)`

A small client-side translation looks like this:

```rust
use agent_client_protocol as acp;
use acp::schema::{
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionNotification,
};

#[tokio::main]
async fn main() -> acp::Result<()> {
    let transport = todo!("create the transport that connects to your agent");

    acp::Client
        .builder()
        .on_receive_request(
            async move |_: RequestPermissionRequest, responder, _cx| {
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                println!("{:?}", notification.update);
                Ok(())
            },
            acp::on_receive_notification!(),
        )
        .connect_with(transport, async |_cx: acp::ConnectionTo<acp::Agent>| {
            // send requests here, e.g. `_cx.send_request(...).block_task().await?`
            Ok(())
        })
        .await
}
```

## 6. Replace `Agent` trait impls with builder callbacks

The same shift applies on the agent side.

### Common agent-side method mapping

- `initialize` -> `.on_receive_request(|req: InitializeRequest, responder, cx| ...)`
- `authenticate` -> `.on_receive_request(|req: AuthenticateRequest, responder, cx| ...)`
- `new_session` -> `.on_receive_request(|req: NewSessionRequest, responder, cx| ...)`
- `prompt` -> `.on_receive_request(|req: PromptRequest, responder, cx| ...)`
- `cancel` -> `.on_receive_notification(|notif: CancelNotification, cx| ...)`
- `load_session` -> `.on_receive_request(|req: LoadSessionRequest, responder, cx| ...)`
- `set_session_mode` -> `.on_receive_request(|req: SetSessionModeRequest, responder, cx| ...)`
- `set_session_config_option` -> `.on_receive_request(|req: SetSessionConfigOptionRequest, responder, cx| ...)`
- `list_sessions` and other unstable session methods -> request handlers for the corresponding schema type
- `ext_method` / `ext_notification` -> your own derived `JsonRpcRequest` / `JsonRpcNotification` types, or a catch-all `on_receive_dispatch(...)`

A minimal agent skeleton now looks like this:

```rust
use agent_client_protocol as acp;
use acp::schema::{
    AgentCapabilities, CancelNotification, InitializeRequest, InitializeResponse,
    PromptRequest, PromptResponse, StopReason,
};
use acp::{Client, Dispatch};

#[tokio::main]
async fn main() -> acp::Result<()> {
    let outgoing = todo!("create the agent's outgoing byte stream");
    let incoming = todo!("create the agent's incoming byte stream");

    acp::Agent
        .builder()
        .name("my-agent")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: PromptRequest, responder, _cx| {
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_notification: CancelNotification, _cx| {
                Ok(())
            },
            acp::on_receive_notification!(),
        )
        .connect_to(acp::ByteStreams::new(outgoing, incoming))
        .await
}
```

If you need a catch-all handler, use `on_receive_dispatch(...)`.

## 7. Replace `subscribe()` with session readers or explicit callbacks

There is no direct connection-level replacement for `ClientSideConnection::subscribe()`.

Choose the replacement based on what you were using it for:

- if you were reading prompt output for one session, prefer `ActiveSession::read_update()` or `ActiveSession::read_to_string()`
- if you were observing inbound notifications generally, register `on_receive_notification(...)`
- if you were forwarding or inspecting raw messages in a proxy, use `on_receive_dispatch(...)` plus `send_proxied_message(...)`

## 8. Remove `LocalSet`, `spawn_local`, and manual I/O tasks

The old crate examples needed a `LocalSet` because the connection futures were `!Send`.

Most migrations to the `0.11` API can remove:

- `tokio::task::LocalSet`
- `tokio::task::spawn_local(...)`
- the custom `spawn` closure passed into connection construction
- the separate `handle_io` future you had to drive manually

When you need concurrency from a handler in `0.11`, use `cx.spawn(...)`.

## 9. Prefer `agent-client-protocol-tokio` for subprocess agents

If your old client code spawned an agent with `tokio::process::Command`, the new stack has a higher-level helper for that. `AcpAgent` implements `ConnectTo<Client>` and takes care of spawning the process and wiring up its stdio:

```rust
use agent_client_protocol as acp;
use acp::schema::{InitializeRequest, ProtocolVersion};
use agent_client_protocol_tokio::AcpAgent;
use std::str::FromStr;

#[tokio::main]
async fn main() -> acp::Result<()> {
    let agent = AcpAgent::from_str("python my_agent.py")?;

    acp::Client
        .builder()
        .name("my-client")
        .connect_with(agent, async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            // ...send more requests, build a session, etc.
            Ok(())
        })
        .await
}
```

Use `connect_to(agent)` (without a `main_fn`) only when the client has no
outbound requests to send and just needs to keep the connection alive so
registered handlers can respond to the agent. In that mode the builder
runs until the transport closes or a handler returns an error.

You can still use `ByteStreams::new(...)` when you already own the byte
streams and do not want the extra helper crate.

## 10. Common gotchas

- `Client` and `Agent` are role markers now, not traits you implement.
- The response type for `send_request(...)` is inferred from the request type.
- `send_request(...)` does not wait by itself; use `.block_task().await?` or `on_receiving_result(...)`.
- Be careful about calling blocking operations from `on_receive_*` callbacks. Those callbacks run in the dispatch loop and preserve message ordering.
- If your old code used `subscribe()` as a global message tap, plan a new strategy around `ActiveSession`, notification callbacks, or proxy dispatch handlers.
- For reusable ACP components, implement `ConnectTo<Role>` instead of trying to recreate the old monolithic trait pattern.
