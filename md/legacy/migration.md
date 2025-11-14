# Migration Guide

This guide helps you migrate from the legacy `agent-client-protocol` crate to the new `sacp` crate.

## Why Migrate?

The `sacp` crate offers several advantages over the legacy SDK:

- **Handler-based architecture**: More flexible message handling with composable handler chains
- **Better async ergonomics**: Cleaner patterns for non-blocking operations
- **Proxy support**: Built-in support for building ACP proxies and middleware
- **Type-driven dispatch**: Handlers are automatically dispatched based on message types
- **Active development**: New features and improvements are happening in the SACP crates

## Key Differences

| Aspect | Legacy (`agent-client-protocol`) | New (`sacp`) |
|--------|----------------------------------|--------------|
| **API Style** | Trait-based (implement `Agent` or `Client`) | Handler-based (register callbacks with `JrHandlerChain`) |
| **Connection Setup** | Explicit connection with spawn function | Connection handled by `serve()` or `with_client()` |
| **Message Handling** | Implement all trait methods | Register handlers only for messages you care about |
| **Sending Requests** | Call methods on connection object | Use `cx.send_request()` from handler context |

## Migration Steps

### 1. Setting Up a Connection

**Legacy approach:**

```rust,ignore
{{#include ../../examples/migration/agent_legacy.rs:connection}}
```

**New approach:**

```rust,ignore
{{#include ../../examples/migration/agent_sacp.rs:setup}}
```

**Key changes:**
- No need to manually manage the spawn function or IO task
- Use `.serve()` for agents/servers or `.with_client()` for clients
- The connection lifecycle is managed automatically

### 2. Converting from Trait Implementation to Handler Chain

#### For Agents

**Legacy approach - Implement the `Agent` trait:**

```rust,ignore
{{#include ../../examples/migration/agent_legacy.rs:setup}}
```

**New approach - Register handlers:**

```rust,ignore
{{#include ../../examples/migration/agent_sacp.rs:handler_initialize}}
```

```rust,ignore
{{#include ../../examples/migration/agent_sacp.rs:handler_new_session}}
```

```rust,ignore
{{#include ../../examples/migration/agent_sacp.rs:handler_prompt}}
```

**Key changes:**
- Use `.on_receive_request()` to register a handler for each request type
- Handlers are called in order - first matching handler wins
- You only need to register handlers for messages you actually handle
- Use `.on_receive_message()` as a fallback for unhandled messages

#### For Clients

**Legacy approach - Implement the `Client` trait:**

```rust,ignore
{{#include ../../examples/migration/client_legacy.rs:client_impl}}
```

**New approach - Register handlers:**

```rust,ignore
{{#include ../../examples/migration/client_sacp.rs:handler_permission}}
```

```rust,ignore
{{#include ../../examples/migration/client_sacp.rs:handler_file_ops}}
```

```rust,ignore
{{#include ../../examples/migration/client_sacp.rs:handler_notifications}}
```

### 3. Sending Requests and Awaiting Responses

**Legacy approach:**

```rust,ignore
{{#include ../../examples/migration/client_legacy.rs:send_requests}}
```

**New approach:**

```rust,ignore
{{#include ../../examples/migration/client_sacp.rs:with_client}}
```

**Key changes:**
- Use `.with_client()` to get a connection context for sending requests
- Call `cx.send_request()` to send requests
- Use `.block_task()` to wait for the response
- The `async` block in `with_client()` has access to the connection context

### 4. Avoiding Blocking in Handlers

⚠️ **Important:** Message handlers run on the event loop. Blocking in a handler prevents the connection from processing new messages.

This is especially important when you need to send a request to the other side from within a handler:

```rust,ignore
{{#include ../../examples/migration/agent_with_callbacks_sacp.rs:blocking_risk}}
```

**Alternatives:**

1. **Use `await_when_*` methods** to defer work until after responding:

```rust,ignore
{{#include ../../examples/migration/agent_with_callbacks_sacp.rs:blocking_risk}}
```

2. **Use `cx.spawn()` for concurrent background work:**

```rust,ignore
{{#include ../../examples/migration/agent_with_callbacks_sacp.rs:spawn_alternative}}
```

## Breaking Changes

### Type Names

- `ClientSideConnection` → Use `JrHandlerChain` with `.with_client()`
- `AgentSideConnection` → Use `JrHandlerChain` with `.serve()`
- Schema types moved from `agent_client_protocol_schema` to `sacp::schema`

### Connection Pattern

The legacy SDK requires you to:
1. Create a connection with a spawn function
2. Manually spawn the IO task
3. Store the connection object

The new SDK handles all of this automatically:
- Just call `.serve()` or `.with_client()` and it manages the connection lifecycle

### Message Handling

The legacy SDK requires implementing all methods of the `Agent` or `Client` trait (returning errors for unimplemented ones).

The new SDK only requires registering handlers for messages you actually handle. Unhandled messages automatically return a "method not found" error.

### Request Context

In the legacy SDK, you call methods on a connection object to send messages.

In the new SDK, you use the context (`cx`) provided to your handler:
- `cx.respond()` - respond to the current request
- `cx.send_request()` - send a new request
- `cx.send_notification()` - send a notification
- `cx.spawn()` - spawn background work
- `cx.connection_cx()` - get a context not tied to a specific request

## Next Steps

- See the [Crate Reference](../sacp/crates.md) for detailed documentation
- Check out [examples](https://github.com/nikomatsakis/acp-rust-sdk/tree/main/examples/migration) for complete working code
- Learn about [proxies](../sacp/architecture.md#proxies) for advanced use cases
