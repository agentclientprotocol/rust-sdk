//! Transport-level error objects, before choosing an application protocol.

use agent_client_protocol_schema::MaybeUndefined;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A JSON-RPC error without ACP-specific interpretation.
///
/// Raw transports and relays preserve unknown fields and distinguish omitted
/// `data` from explicit JSON null. Convert to [`crate::Error`] only when
/// dispatching an ACP response; other protocols have their own error domains.
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
    /// discards `extra`. Do not use it when forwarding raw frames or interpreting
    /// errors from another protocol.
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
    use futures::StreamExt as _;
    use serde_json::json;

    #[test]
    fn raw_errors_preserve_codes_data_presence_and_extensions() {
        for code in [i32::MIN, -32000, -32602, 0, 12345, i32::MAX] {
            for data in [None, Some(Value::Null), Some(json!({"detail":[1,2]}))] {
                let mut error = json!({
                    "code": code,
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
                assert_eq!(error.code, code);
                match data {
                    None => assert!(error.data.is_undefined()),
                    Some(Value::Null) => assert!(error.data.is_null()),
                    Some(value) => assert_eq!(error.data.value(), Some(&value)),
                }
            }
        }
    }

    #[tokio::test]
    async fn raw_responses_survive_framing_and_channel_relay() {
        let error = json!({"code":-32000, "message":"peer", "extension":{"retry":true}});
        for wire in [
            json!({"jsonrpc":"2.0", "id":"error", "error":error}),
            json!([
                {"jsonrpc":"2.0", "id":"omitted", "error":error},
                {"jsonrpc":"2.0", "id":"null", "error":{
                    "code":12345, "message":"peer", "data":null, "_meta":{"opaque":"kept"}
                }},
                {"jsonrpc":"2.0", "id":"success", "result":null}
            ]),
        ] {
            let frame = TransportFrame::parse_json(&wire.to_string());
            let (source, relay_in) = Channel::duplex();
            let (relay_out, mut destination) = Channel::duplex();
            source.tx.unbounded_send(frame).unwrap();
            drop(source);
            Channel {
                rx: relay_in.rx,
                tx: relay_out.tx,
            }
            .copy()
            .await
            .unwrap();
            let received = destination.rx.next().await.unwrap();
            let received: Value = serde_json::from_str(&received.to_json().unwrap()).unwrap();
            assert_eq!(received, wire);
        }
    }

    #[test]
    fn acp_error_interpretation_is_explicit_and_keeps_data_presence() {
        for (code, expected_code) in [
            (-32000, crate::ErrorCode::AuthRequired),
            (12345, crate::ErrorCode::Other(12345)),
        ] {
            for data in [None, Some(Value::Null), Some(json!({"detail":"kept"}))] {
                let mut raw = RawJsonRpcError::new(code, "peer");
                raw.extra.insert("extension".into(), json!(true));
                if let Some(data) = &data {
                    raw = raw.data(data.clone());
                }
                let error = raw.into_acp_error();
                assert_eq!(error.code, expected_code);
                assert_eq!(error.message, "peer");
                assert_eq!(error.data, data);
                let roundtrip = RawJsonRpcError::from(error);
                assert_eq!(roundtrip.code, code);
                assert!(roundtrip.extra.is_empty());
                match data {
                    None => assert!(roundtrip.data.is_undefined()),
                    Some(Value::Null) => assert!(roundtrip.data.is_null()),
                    Some(value) => assert_eq!(roundtrip.data.value(), Some(&value)),
                }
            }
        }
    }

    #[test]
    fn acp_response_constructor_preserves_explicit_null_data() {
        for data in [None, Some(Value::Null), Some(json!({"detail":"kept"}))] {
            let mut error = crate::Error::invalid_params();
            error.data = data.clone();
            let message =
                RawJsonRpcMessage::response(crate::schema::v1::RequestId::Null, Err(error));
            let wire = serde_json::to_value(message).unwrap();
            assert_eq!(wire["error"]["code"], -32602);
            assert_eq!(wire["error"].get("data"), data.as_ref());
        }
    }

    #[test]
    fn malformed_raw_errors_are_still_rejected() {
        for error in [
            Value::Null,
            json!({"code":-32000}),
            json!({"message":"peer"}),
            json!({"code":null, "message":"peer"}),
            json!({"code":"-32000", "message":"peer"}),
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
