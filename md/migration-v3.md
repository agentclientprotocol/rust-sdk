# Migrating to Core v3

The upcoming combined breaking release pairs `agent-client-protocol` 3.x with
`agent-client-protocol-rmcp` 4.x and `rmcp` 3.x. Applications using the
integration must upgrade both public dependencies together; see the
[rmcp integration migration](./migration-rmcp-v4.md) for service and tool API
changes. These are planned release versions, not the workspace's current
package versions. The minimum supported Rust version remains 1.88.

For the other transport changes in core 3.x, follow:

- [Migrating Connection Drivers](./migration-connection-drivers.md) for
  `(Channel, Option<ConnectionDriver>)` and active/passive transport ownership.
- [Migrating the Native MCP Binding](./migration-stateless-mcp.md) for the
  replacement of connect/disconnect with request-scoped MCP 2026-07-28.
- [Migrating Custom HTTP Client Construction](./http-transport.md#migrating-custom-client-construction)
  for WebSocket configuration through the transport builder instead of a prebuilt
  reqwest client.
- [Native MCP-over-ACP](./mcp-over-acp.md) for request-scoped provider usage and
  [the MCP HTTP bridge](./mcp-bridge.md) for modern-only HTTP re-export.

For Cargo feature selection and defaults, see [Cargo Features](./features.md).

For non-exhaustive server options and CORS policies, see
[Migrating the Config API](./migration-config-api.md). Agent subprocess config
constructors and fluent methods remain unchanged.

## Raw responses are no longer ACP errors

`RawJsonRpcMessage::Response` now contains `RawJsonRpcResponse`, whose error
variant carries `Box<RawJsonRpcError>` instead of an ACP `Error`. Typed ACP
request APIs still return ACP `Error`; raw transports, interceptors, and
relays must use the protocol-neutral type.

Raw error codes are `i32`, not `ErrorCode`. Raw errors distinguish omitted
`data` from explicit JSON null and preserve unknown error-object fields in
`extra`. Do not interpret MCP or other protocols' error codes as ACP codes.

### Construct and match raw responses

Use `RawJsonRpcResponse::new` with a `Result<Value, Box<RawJsonRpcError>>`, or
construct its `Result` and `Error` variants directly. Use the constructor for
the non-exhaustive raw error struct, then set data and extensions:

```rust
use agent_client_protocol::{RawJsonRpcError, RawJsonRpcMessage, RawJsonRpcResponse};
use serde_json::{Value, json};

fn main() {
    let success = RawJsonRpcResponse::new(1_i64, Ok(json!({"answer": 42})));
    assert!(matches!(success, RawJsonRpcResponse::Result { .. }));

    let mut error = RawJsonRpcError::new(-32000, "Tool unavailable").data(Value::Null);
    error.extra.insert("retryable".into(), json!(true));
    let failure = RawJsonRpcResponse::Error {
        id: 2_i64.into(),
        error: Box::new(error),
    };
    let message = RawJsonRpcMessage::Response(failure);

    match message {
        RawJsonRpcMessage::Response(RawJsonRpcResponse::Result { id, result }) => {
            println!("{id}: {result}");
        }
        RawJsonRpcMessage::Response(RawJsonRpcResponse::Error { id, error }) => {
            assert_eq!(error.code, -32000);
            assert!(error.data.is_null());
            assert_eq!(error.extra["retryable"], json!(true));
            println!("{id}: {}", error.message);
        }
        _ => panic!("expected a response"),
    }
}
```

`RawJsonRpcError::new` leaves `data` omitted; `.data(Value::Null)` explicitly
includes `"data": null`. The boxing is required by the response type.

### Convert only at a typed ACP boundary

When a response is known to be ACP, explicitly unbox and consume the raw
error with `into_acp_error`. This interprets ACP error codes, keeps data
presence, and intentionally discards error extension fields:

```rust
use agent_client_protocol::{Error, ErrorCode, RawJsonRpcError, RawJsonRpcResponse};
use serde_json::{Value, json};

fn into_acp_result(response: RawJsonRpcResponse) -> Result<Value, Error> {
    match response {
        RawJsonRpcResponse::Result { result, .. } => Ok(result),
        RawJsonRpcResponse::Error { error, .. } => Err((*error).into_acp_error()),
    }
}

fn main() {
    // This response belongs to ACP, where -32000 means authentication required.
    let mut raw = RawJsonRpcError::new(-32000, "Authentication required").data(Value::Null);
    raw.extra.insert("peerExtension".into(), json!(true));
    let response = RawJsonRpcResponse::new(3_i64, Err(Box::new(raw)));
    let error = into_acp_result(response).unwrap_err();
    assert_eq!(error.code, ErrorCode::AuthRequired);
    assert_eq!(error.data, Some(Value::Null));

    let raw_again = RawJsonRpcError::from(error);
    assert!(raw_again.extra.is_empty()); // Conversion is not a lossless relay.
}
```

For locally generated ACP responses, `RawJsonRpcMessage::response(id,
Result<Value, Error>)` remains available and converts the ACP error to its raw
representation. It is not a constructor for another protocol's errors.

### Relay without ACP interpretation

Forward a raw message unchanged when no ID rewrite is needed. If a relay
maps request IDs, move the original boxed raw error into the new response;
do not round-trip it through `Error` or `RawJsonRpcMessage::response`:

```rust
use agent_client_protocol::{RawJsonRpcMessage, RawJsonRpcResponse};
use serde_json::{Value, json};

fn main() {
    // A downstream protocol's -32000 is not necessarily ACP AuthRequired.
    let wire = json!({
        "jsonrpc": "2.0",
        "id": "downstream",
        "error": {
            "code": -32000,
            "message": "Tool unavailable",
            "data": null,
            "retryable": true
        }
    });
    let message: RawJsonRpcMessage = serde_json::from_value(wire.clone()).unwrap();
    let RawJsonRpcMessage::Response(response) = message else {
        panic!("expected a response");
    };
    let upstream_id = String::from("upstream").into();
    let relayed = match response {
        RawJsonRpcResponse::Result { result, .. } => {
            RawJsonRpcResponse::Result { id: upstream_id, result }
        }
        RawJsonRpcResponse::Error { error, .. } => {
            RawJsonRpcResponse::Error { id: upstream_id, error }
        }
    };

    let actual: Value = serde_json::to_value(RawJsonRpcMessage::Response(relayed)).unwrap();
    let mut expected = wire;
    expected["id"] = json!("upstream");
    assert_eq!(actual, expected); // Code, explicit null, and extensions survive.
}
```

The same relay rule preserves omission when the input has no `data` field.
See the [raw response reference](./protocol.md#raw-json-rpc-responses) for
the protocol boundary.
