# Logging and diagnostic privacy

Ordinary SDK warning logs describe protocol errors using the numeric error code
and a fixed, code-derived reason. Dispatch warnings also identify the method
and, where applicable, request ID and handler. They do not format the error's
application-controlled message or data.

This applies to malformed typed notifications, request and notification handler
failures (including dynamic handlers), and outgoing error responses. Serde parse
reasons can quote invalid strings or enum variants, so merely omitting
`Error.data.json` would not make those reasons safe to log. The SDK does not
extract field paths from free-form error text; there is currently no separately
trusted structured field path to include in these warnings.

## Detailed diagnostics are sensitive

The errors returned to callers and sent to peers are unchanged. In particular,
`util::json_cast` and `util::json_cast_params` can include the complete failing
JSON input and a payload-bearing parse reason. Applications should not log these
errors using their ordinary `Debug` or `Display` representations unless they
intend to record that content.

## DEBUG and TRACE logs

DEBUG logs identify messages by kind, method, and request ID only. They never
format params, results, or error messages and data, so enabling DEBUG does not
record prompts, tool arguments, or credentials. Serde parse failures are logged
by category (and position, for syntax errors) rather than their reason text.

Only the transport-level TRACE events (`Received JSON-RPC message`, `Sending
JSON-RPC message`, `Sending JSON-RPC batch`, and `Relaying invalid JSON-RPC
value`) record complete messages. They redact every value of an `env` or
`headers` field (MCP server declarations in session setup, terminal auth
methods, terminal creation, and provider configuration), keeping the names, and
omit lines that are not valid JSON. Other TRACE events carry metadata only.

Explicitly enabled TRACE logging still records terminal output, prompts, tool
arguments, error responses, and any credentials outside `env`/`headers`. Treat
these traces as private data and enable, store, and share them accordingly.
Application-provided logging callbacks, such as `AcpAgent::with_debug`, receive
raw lines and are not redacted.
