# Testy ACP Test Agent

`testy` is a deterministic ACP agent binary for exercising clients against ACP.
It is built from the `agent-client-protocol-test` crate and communicates over stdio like a normal
agent. Its v1 and draft v2 implementations are native rather than protocol conversions.

The default build enables `agent-client-protocol-test`'s `unstable` cargo feature, which forwards
to the SDK's `unstable` feature and builds the v1 agent:

```bash
cargo build -p agent-client-protocol-test --bin testy
```

Enable the separate draft v2 feature to build a dual-version binary:

```bash
cargo build -p agent-client-protocol-test --bin testy --features unstable_protocol_v2
```

That binary selects v1 or v2 from the client's `initialize` request. `just prep-tests` already
builds Testy with all features, so the prebuilt test binary supports both versions.

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
- `wait_for_cancel` accepts the prompt and waits for `session/cancel`.
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

### Protocol v1

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
deterministic invalid-params prompt error.

### Draft protocol v2

With `unstable_protocol_v2`, the binary also handles the complete advertised v2 session baseline:
`initialize`, `session/new`, `session/list`, `session/resume`, `session/close`, `session/prompt`,
`session/cancel`, and `session/update`.

The v2 implementation follows the split prompt lifecycle:

1. `session/prompt` returns an empty acceptance response.
2. Testy independently sends the accepted user message and a `running` state update.
3. Output arrives through message updates.
4. An `idle` state update with a stop reason completes the foreground work.

`wait_for_cancel` makes this separation deterministic for client tests: prompt acceptance returns
while work remains active, and `session/cancel` is confirmed by `idle` with the `cancelled` stop
reason. Testy also keeps simple message history and replays it before a `session/resume` response
when the client requests replay from the start.

The existing v1 scenarios do not map one-to-one onto v2. V2 scenario parity, client callbacks,
MCP, authentication, deletion, configuration, and other optional capabilities remain unadvertised
for now.
