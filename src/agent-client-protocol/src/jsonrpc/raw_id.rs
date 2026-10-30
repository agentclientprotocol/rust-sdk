//! The protocol-version-neutral JSON-RPC ID used by the raw message layer.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::schema::v1;
#[cfg(feature = "unstable_protocol_v2")]
use crate::schema::v2;

/// A JSON-RPC `id` exactly as it appears on the wire: an integer, a string or
/// `null`.
///
/// This is the ID of [`RawJsonRpcMessage`](crate::RawJsonRpcMessage) and its
/// requests and responses, and of the SDK's request and response handles,
/// whichever ACP version the connection speaks. It keeps the wire form: `1`
/// and `"1"` are different IDs, and integers are kept exactly across the
/// whole 64-bit signed range, which is the range every ACP schema version
/// accepts. Other numbers are not valid IDs.
///
/// `null` is allowed because JSON-RPC allows it, and protocol v1 accepts it
/// on requests. Protocol v2 does not; the SDK rejects a v2 request with a
/// `null` ID before dispatch. Error responses to messages whose ID cannot be
/// read also carry `null`.
///
/// It converts losslessly to and from [`v1::RequestId`], and from
/// `v2::RequestId`. Converting to `v2::RequestId` fails only for `null`:
///
/// ```
/// use agent_client_protocol::{RawJsonRpcId, schema::v1};
///
/// let id = RawJsonRpcId::from(v1::RequestId::Number(7));
/// assert_eq!(id, RawJsonRpcId::Number(7));
/// assert_eq!(v1::RequestId::from(id), v1::RequestId::Number(7));
/// assert_eq!(serde_json::to_string(&RawJsonRpcId::from("7".to_string())).unwrap(), r#""7""#);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(untagged)]
#[allow(
    clippy::exhaustive_enums,
    reason = "JSON-RPC defines exactly these ID types"
)]
pub enum RawJsonRpcId {
    /// The JSON-RPC `null` ID.
    Null,
    /// An integer ID.
    Number(i64),
    /// A string ID.
    Str(String),
}

impl fmt::Display for RawJsonRpcId {
    /// Integers and strings are written without quotes, and `null` as `null`,
    /// as [`v1::RequestId`] does. Use `serde_json` for the wire form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Number(id) => write!(f, "{id}"),
            Self::Str(id) => f.write_str(id),
        }
    }
}

impl From<i64> for RawJsonRpcId {
    fn from(id: i64) -> Self {
        Self::Number(id)
    }
}

impl From<String> for RawJsonRpcId {
    fn from(id: String) -> Self {
        Self::Str(id)
    }
}

impl From<v1::RequestId> for RawJsonRpcId {
    fn from(id: v1::RequestId) -> Self {
        match id {
            v1::RequestId::Null => Self::Null,
            v1::RequestId::Number(id) => Self::Number(id),
            v1::RequestId::Str(id) => Self::Str(id),
        }
    }
}

impl From<RawJsonRpcId> for v1::RequestId {
    fn from(id: RawJsonRpcId) -> Self {
        match id {
            RawJsonRpcId::Null => Self::Null,
            RawJsonRpcId::Number(id) => Self::Number(id),
            RawJsonRpcId::Str(id) => Self::Str(id),
        }
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl From<v2::RequestId> for RawJsonRpcId {
    fn from(id: v2::RequestId) -> Self {
        match id {
            v2::RequestId::Number(id) => Self::Number(id),
            v2::RequestId::Str(id) => Self::Str(id),
        }
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl TryFrom<RawJsonRpcId> for v2::RequestId {
    type Error = crate::Error;

    /// Fails for `null`, which protocol v2 does not allow, with the Invalid
    /// Request error (`-32600`) the SDK sends for a v2 request with a `null`
    /// ID.
    fn try_from(id: RawJsonRpcId) -> Result<Self, Self::Error> {
        match id {
            RawJsonRpcId::Null => Err(null_v2_request_id()),
            RawJsonRpcId::Number(id) => Ok(Self::Number(id)),
            RawJsonRpcId::Str(id) => Ok(Self::Str(id)),
        }
    }
}

/// The Invalid Request error for a protocol-v2 request whose ID is `null`.
pub(crate) fn null_v2_request_id() -> crate::Error {
    crate::Error::invalid_request()
        .data("ACP protocol v2 request ids must be a string or an integer, not null")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn wire_ids() -> Vec<Value> {
        vec![
            Value::Null,
            json!(0),
            json!(1),
            json!(-1),
            json!(9_007_199_254_740_993_i64),
            json!(i64::MAX),
            json!(i64::MIN),
            json!("1"),
            json!(""),
            json!("ünïcødé \"quoted\""),
        ]
    }

    #[test]
    fn wire_form_round_trips_exactly() {
        for wire in wire_ids() {
            let text = wire.to_string();
            let id: RawJsonRpcId = serde_json::from_str(&text).unwrap();
            assert_eq!(serde_json::to_string(&id).unwrap(), text);
        }
        assert_ne!(
            serde_json::from_str::<RawJsonRpcId>("1").unwrap(),
            serde_json::from_str::<RawJsonRpcId>(r#""1""#).unwrap()
        );
    }

    #[test]
    fn non_ids_are_rejected() {
        for text in [
            "9223372036854775808",
            "-9223372036854775809",
            "1.5",
            "1e3",
            "true",
            "[]",
            "{}",
        ] {
            assert!(
                serde_json::from_str::<RawJsonRpcId>(text).is_err(),
                "{text}"
            );
            assert!(
                serde_json::from_str::<v1::RequestId>(text).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn v1_conversions_are_lossless_and_match_the_wire() {
        for wire in wire_ids() {
            let v1_id: v1::RequestId = serde_json::from_value(wire.clone()).unwrap();
            let id = RawJsonRpcId::from(v1_id.clone());
            assert_eq!(serde_json::to_value(&id).unwrap(), wire);
            assert_eq!(id.to_string(), v1_id.to_string());
            assert_eq!(v1::RequestId::from(id), v1_id);
        }
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn v2_conversions_are_lossless_and_reject_null() {
        for wire in wire_ids() {
            let id: RawJsonRpcId = serde_json::from_value(wire.clone()).unwrap();
            if let Ok(v2_id) = serde_json::from_value::<v2::RequestId>(wire.clone()) {
                assert_eq!(RawJsonRpcId::from(v2_id.clone()), id);
                assert_eq!(v2::RequestId::try_from(id).unwrap(), v2_id);
            } else {
                assert_eq!(id, RawJsonRpcId::Null);
                let error = v2::RequestId::try_from(id).unwrap_err();
                assert_eq!(error.code, crate::ErrorCode::INVALID_REQUEST);
            }
        }
    }
}
