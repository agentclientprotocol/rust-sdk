# agent-client-protocol-derive

Procedural derives for custom JSON-RPC messages in the
[Agent Client Protocol Rust SDK](https://docs.rs/agent-client-protocol).

The core `agent-client-protocol` crate re-exports these macros, so applications
normally do not need a direct dependency on this crate.

```toml
[dependencies]
agent-client-protocol = "3"
serde = { version = "1", features = ["derive"] }
```

```rust
use agent_client_protocol::{JsonRpcMessage, JsonRpcRequest, JsonRpcResponse};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_example/hello", response = HelloResponse)]
struct HelloRequest {
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct HelloResponse {
    greeting: String,
}

fn main() {
    let request = HelloRequest { name: "Ada".into() };
    assert_eq!(request.method(), "_example/hello");
}
```

`JsonRpcNotification` provides the corresponding notification derive through
`#[notification(method = "...")]`. The derives use Serde for message bodies
and do not require the core's `schemars` feature.

Custom crate paths are supported by the `crate` attribute on `request`,
`notification`, and `response`. See the
[macro reference](https://docs.rs/agent-client-protocol-derive) for attributes
and generic-type support.

## License

Apache-2.0. The published package includes `LICENSE`.
