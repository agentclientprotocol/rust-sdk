# Crate Reference

## Crate Overview

### `sacp`

Base JSON-RPC protocol support for implementing ACP clients and agents. Provides core types, message handling, and the foundation for building ACP components.

[Documentation](https://docs.rs/sacp/latest/sacp/)

### `sacp-proxy`

Extensions for building proxy components that sit between clients and agents. Provides the `_proxy/successor/*` protocol implementation and traits for handling proxied messages.

[Documentation](https://docs.rs/sacp-proxy/latest/sacp_proxy/)

### `sacp-conductor`

Orchestrator for managing proxy chains. Spawns component processes, routes messages through the chain, and handles capability negotiation.

[Documentation](https://docs.rs/sacp-conductor/latest/sacp_conductor/)

### `sacp-tokio`

Tokio-specific utilities and helpers for building async ACP components using the Tokio runtime.

[Documentation](https://docs.rs/sacp-tokio/latest/sacp_tokio/)

## Examples

### `sacp-tee`

A debugging proxy that logs all ACP traffic passing through it to a file. Useful for debugging proxy chains and understanding message flow. Acts as a transparent pass-through while keeping detailed logs of all requests, responses, and notifications.

[Source](https://github.com/agentclientprotocol/rust-sdk/tree/main/src/sacp-tee)

### `elizacp`

A simple agent implementation using the classic ELIZA chatbot algorithm. Provides deterministic, pattern-based responses which makes it ideal for testing proxy chains and protocol implementations without requiring an actual AI model.

[Source](https://github.com/agentclientprotocol/rust-sdk/tree/main/src/elizacp)

### `yopo`

"You Only Prompt Once" - A minimal ACP client that sends a single prompt and exits. Useful for quick testing and as a reference implementation for building clients.

[Source](https://github.com/agentclientprotocol/rust-sdk/tree/main/src/yopo)

## Recommendations

### Building a Client

Use `sacp` for the core protocol implementation. Add `sacp-tokio` if you're using the Tokio async runtime.

See [`yopo`](https://github.com/agentclientprotocol/rust-sdk/tree/main/src/yopo) for a minimal example.

### Building an Agent

Use `sacp` for the core protocol implementation. Add `sacp-tokio` if you're using the Tokio async runtime.

See [`elizacp`](https://github.com/agentclientprotocol/rust-sdk/tree/main/src/elizacp) for a simple example.

### Building a Proxy

Use `sacp-proxy` for proxy-specific protocol support. Add `sacp-tokio` if you're using the Tokio async runtime.

See [`sacp-tee`](https://github.com/agentclientprotocol/rust-sdk/tree/main/src/sacp-tee) for a transparent logging proxy example.

### Building an Orchestrator

Use `sacp-conductor` to manage proxy chains, along with `sacp-proxy` if you need to implement custom proxy logic within the orchestrator.
