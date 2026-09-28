//! Transport-level error objects, before choosing an application protocol.

use agent_client_protocol_schema::MaybeUndefined;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A JSON-RPC error without ACP-specific interpretation.
///
/// Raw transports and relays preserve unknown fields and distinguish omitted
/// `data` from explicit JSON null. Convert to [`crate::Error`] only when
/// dispatching an ACP response; an MCP error code belongs to a different domain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RawJsonRpcError {
    /// The peer's numeric error code, not an ACP [`crate::ErrorCode`].
    pub code: i32,
    /// The peer's error message.
    pub message: String,
    /// Optional error data. Explicit null is retained separately from omission.
    #[serde(default, skip_serializing_if = "MaybeUndefined::is_undefined")]
    pub data: MaybeUndefined<Value>,
    /// Additional fields on the error object.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A transport-level JSON-RPC response with an opaque result or raw error.
///
/// Errors are boxed so their extensible representation does not enlarge every
/// request, notification, and queued frame.
pub type RawJsonRpcResponse =
    agent_client_protocol_schema::rpc::Response<Value, Box<RawJsonRpcError>>;

impl RawJsonRpcError {
    /// Construct an error without data or extension fields.
    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: MaybeUndefined::Undefined,
            extra: Map::new(),
        }
    }

    /// Set error data, preserving explicit null.
    #[must_use]
    pub fn data(mut self, data: Value) -> Self {
        self.data = if data.is_null() {
            MaybeUndefined::Null
        } else {
            MaybeUndefined::Value(data)
        };
        self
    }

    /// Interpret this error as an ACP response for the typed dispatcher.
    ///
    /// ACP's error type does not model extension fields, so this intentionally
    /// discards `extra`. Do not use it when forwarding raw frames or projecting
    /// errors from another protocol such as MCP.
    #[must_use]
    pub fn into_acp_error(self) -> crate::Error {
        let mut error = crate::Error::new(self.code, self.message);
        error.data = match self.data {
            MaybeUndefined::Undefined => None,
            MaybeUndefined::Null => Some(Value::Null),
            MaybeUndefined::Value(data) => Some(data),
        };
        error
    }
}

impl From<crate::Error> for RawJsonRpcError {
    fn from(error: crate::Error) -> Self {
        let raw = Self::new(error.code.into(), error.message);
        match error.data {
            Some(data) => raw.data(data),
            None => raw,
        }
    }
}

impl std::fmt::Display for RawJsonRpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for RawJsonRpcError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Channel, RawJsonRpcMessage, TransportFrame};
    use futures::{SinkExt as _, StreamExt as _};
    use serde_json::json;

    #[test]
    fn raw_errors_preserve_omission_null_values_and_extensions() {
        for data in [None, Some(Value::Null), Some(json!({"detail":[1,2]}))] {
            let mut error = json!({
                "code": -32000,
                "message": "peer",
                "extension": {"retry": true},
                "_meta": {"opaque": "kept"}
            });
            if let Some(data) = &data {
                error["data"] = data.clone();
            }
            let wire = json!({"jsonrpc":"2.0", "id":"logical", "error":error});
            let parsed: RawJsonRpcMessage = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(&parsed).unwrap(), wire);
            let RawJsonRpcMessage::Response(RawJsonRpcResponse::Error { error, .. }) = parsed
            else {
                panic!("expected a raw error response");
            };
            assert_eq!(error.code, -32000);
            match data {
                None => assert!(error.data.is_undefined()),
                Some(Value::Null) => assert!(error.data.is_null()),
                Some(value) => assert_eq!(error.data.value(), Some(&value)),
            }
        }
    }

    #[tokio::test]
    async fn raw_error_batch_survives_framing_and_budgeted_relay() {
        let wire = json!([
            {"jsonrpc":"2.0", "id":"error", "error":{
                "code":-32000, "message":"peer", "data":null, "extension":{"retry":true}
            }},
            {"jsonrpc":"2.0", "id":"success", "result":null}
        ]);
        let frame = TransportFrame::parse_json(&wire.to_string());
        assert!(matches!(&frame, TransportFrame::Batch(_)));
        let (source, mut relay_in) = Channel::duplex();
        let (mut relay_out, mut destination) = Channel::duplex();
        source.tx.send_frame(frame).await.unwrap();
        let admitted = relay_in.rx.next().await.unwrap();
        relay_out.tx.send(admitted).await.unwrap();
        let received = destination.rx.next().await.unwrap();
        let received: Value = serde_json::from_str(&received.frame().to_json().unwrap()).unwrap();
        assert_eq!(received, wire);
    }

    #[test]
    fn acp_error_interpretation_is_explicit_and_keeps_data_presence() {
        let raw = RawJsonRpcError::new(-32000, "peer");
        assert_eq!(raw.clone().into_acp_error().data, None);
        let error = raw.data(Value::Null).into_acp_error();
        assert_eq!(error.code, crate::ErrorCode::AuthRequired);
        assert_eq!(error.data, Some(Value::Null));
        let roundtrip = RawJsonRpcError::from(error);
        assert!(roundtrip.data.is_null());
        assert!(roundtrip.extra.is_empty());
    }

    #[test]
    fn malformed_raw_errors_are_still_rejected() {
        for error in [
            Value::Null,
            json!({"code":-32000}),
            json!({"message":"peer"}),
            json!({"code":null, "message":"peer"}),
            json!({"code":1.5, "message":"peer"}),
            json!({"code":-32000, "message":null}),
        ] {
            assert!(
                serde_json::from_value::<RawJsonRpcMessage>(
                    json!({"jsonrpc":"2.0", "id":1, "error":error})
                )
                .is_err()
            );
        }
    }
}
