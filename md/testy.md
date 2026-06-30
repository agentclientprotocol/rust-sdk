# Testy ACP Test Agent

`testy` is a deterministic ACP agent binary for exercising clients against the stable ACP v1 surface.
It is built from the `agent-client-protocol-test` crate and communicates over stdio like a normal agent.

The default build enables `agent-client-protocol-test`'s `unstable` cargo feature, which forwards
to the SDK's `unstable` feature:

```bash
cargo build -p agent-client-protocol-test --bin testy
```

To build stable-only coverage:

```bash
cargo build -p agent-client-protocol-test --bin testy --no-default-features
```

The binary lands at `target/debug/testy`. Integration tests that need to spawn it should use
`agent_client_protocol_test::test_binaries::testy()` after prebuilding test binaries.

## Prompt Commands

Prompt text can be either plain text or a JSON-serialized `TestyCommand`.

Plain-text commands:

- `help` returns the supported commands and scenarios.
- `echo <message>` streams `<message>` back.
- `session_updates` emits every stable `session/update` variant.
- `content` emits prompt/content-focused updates, including every stable `ContentBlock` variant.
- `tool_calls` emits tool call create and update flows.
- `callbacks` sends every stable agent-to-client request.
- `elicitations` sends only unstable elicitation requests when built with default features.
- `cancel_status` reports whether `session/cancel` has been received.
- `full` runs all stable scenarios in deterministic order.

With default features, `callbacks` and `full` also run unstable protocol coverage.

JSON command form:

```json
{"command":"run_scenario","scenario":"elicitations"}
```

## Coverage

The binary handles every stable client-to-agent v1 request and notification:
`initialize`, `authenticate`, `logout`, `session/new`, `session/load`, `session/list`,
`session/delete`, `session/resume`, `session/close`, `session/set_mode`,
`session/set_config_option`, `session/prompt`, and `session/cancel`.

The `full` scenario sends every stable agent-to-client callback request:
`session/request_permission`, `fs/write_text_file`, `fs/read_text_file`, `terminal/create`,
`terminal/output`, `terminal/wait_for_exit`, `terminal/kill`, and `terminal/release`.
It also emits the stable session update variants, including message chunks, tool calls, plans,
available commands, mode/config/session info, and usage.

With default features, `elicitations`, `callbacks`, and `full` cover `elicitation/create` form mode,
URL mode, session scope, request scope, accept, decline, cancel, and `elicitation/complete`.
If the client advertises form elicitation but not URL elicitation, the URL part returns a
`UrlElicitationRequired` prompt error with deterministic error data.
