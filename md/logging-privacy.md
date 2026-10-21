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

Explicitly enabled protocol TRACE logging can record complete messages,
including terminal output, prompts, tool arguments, and error responses. Treat
these traces as private data and enable, store, and share them accordingly.
This warning-log policy does not redact opt-in DEBUG/TRACE diagnostics or
application-provided logging callbacks.
