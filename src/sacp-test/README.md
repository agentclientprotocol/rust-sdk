# sacp-test

Test utilities and mock types for testing ACP agents and proxies.

## What's in this crate?

This crate provides helpers for writing tests and documentation examples:

- **Mock types** - Pre-defined request, response, and notification types (`MyRequest`, `ProcessRequest`, `SessionUpdate`, etc.)
- **`MockTransport`** - A transport for doctests that don't need to actually run
- **`test_client::yolo_prompt`** - A helper function to connect to an agent, send a prompt, and collect the response
- **`arrow_proxy`** - An example proxy for integration tests
- **Helper functions** - `mock_connection()`, `expensive_analysis()`, and other utilities for examples

## Usage

### Integration testing with `yolo_prompt`

```rust
use sacp_test::test_client::yolo_prompt;

let (write, read) = create_streams_to_agent();
let result = yolo_prompt(write, read, "Hello, agent!").await?;
assert!(result.contains("response text"));
```

### Using mock types in tests

```rust
use sacp_test::{MyRequest, MyResponse, ProcessRequest, ProcessResponse};

// Mock types implement JrRequest/JrNotification traits
// so they can be used directly with JrHandlerChain
```

## When to use this crate

Use `sacp-test` when you need to:
- Write integration tests for ACP agents or proxies
- Provide runnable examples in documentation
- Build test fixtures that need mock ACP message types

This crate is not published (`publish = false`) and is intended for internal testing only.

## Related Crates

- **[sacp](../sacp/)** - Core ACP SDK
- **[sacp-conductor](../sacp-conductor/)** - Uses this crate for integration tests
- **[sacp-proxy](../sacp-proxy/)** - Framework for building ACP proxies

## License

MIT OR Apache-2.0
