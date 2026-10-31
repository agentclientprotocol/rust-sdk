//! Protocol types for proxy communication.
//!
//! These types are intended to become part of the ACP protocol specification.

use crate::schema::IntoOption;
use crate::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, UntypedMessage};
use serde::{Deserialize, Serialize};

// =============================================================================
// Successor forwarding protocol
// =============================================================================

/// JSON-RPC method name for successor forwarding.
pub const METHOD_SUCCESSOR_MESSAGE: &str = "proxy/successor";

/// A message being sent to the successor component.
///
/// Used in `proxy/successor` when the proxy wants to forward a message downstream.
/// The inner message's `method` and `params` are flattened into the envelope's
/// params, next to the optional `_meta` object.
///
/// Build one with [`SuccessorMessage::new`] and [`SuccessorMessage::meta`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SuccessorMessage<M: JsonRpcMessage = UntypedMessage> {
    /// The message to be sent to the successor component.
    #[serde(flatten)]
    pub message: M,

    /// Metadata for the envelope itself, sent as `_meta`. The inner message
    /// keeps its own `_meta`. A received `_meta` that is not an object is
    /// ignored.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_meta_or_none"
    )]
    pub meta: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Read `_meta` as an object, treating any other value as absent, the same way
/// the schema crate's `DefaultOnError` handles `_meta`.
fn deserialize_meta_or_none<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        Some(serde_json::Value::Object(map)) => Ok(Some(map)),
        _ => Ok(None),
    }
}

impl<M: JsonRpcMessage> SuccessorMessage<M> {
    /// Wrap `message` for the successor, without envelope metadata.
    #[must_use]
    pub fn new(message: M) -> Self {
        Self {
            message,
            meta: None,
        }
    }

    /// Set the envelope's `_meta` object, replacing any existing metadata.
    #[must_use]
    pub fn meta(
        mut self,
        meta: impl IntoOption<serde_json::Map<String, serde_json::Value>>,
    ) -> Self {
        self.meta = meta.into_option();
        self
    }
}

impl<M: JsonRpcMessage> JsonRpcMessage for SuccessorMessage<M> {
    fn matches_method(method: &str) -> bool {
        method == METHOD_SUCCESSOR_MESSAGE
    }

    fn method(&self) -> &str {
        METHOD_SUCCESSOR_MESSAGE
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
        UntypedMessage::new(
            METHOD_SUCCESSOR_MESSAGE,
            SuccessorMessage::new(self.message.to_untyped_message()?).meta(self.meta.clone()),
        )
    }

    fn parse_message(method: &str, params: &serde_json::Value) -> Result<Self, crate::Error> {
        if method != METHOD_SUCCESSOR_MESSAGE {
            return Err(crate::Error::method_not_found());
        }
        let outer = crate::util::json_cast_params::<_, SuccessorMessage<UntypedMessage>>(params)?;
        if !M::matches_method(&outer.message.method) {
            return Err(crate::Error::method_not_found());
        }
        let inner = M::parse_message(&outer.message.method, &outer.message.params)?;
        Ok(SuccessorMessage::new(inner).meta(outer.meta))
    }
}

impl<Req: JsonRpcRequest> JsonRpcRequest for SuccessorMessage<Req> {
    type Response = Req::Response;
}

impl<Notif: JsonRpcNotification> JsonRpcNotification for SuccessorMessage<Notif> {}

// =============================================================================
// Proxy initialization protocol
// =============================================================================

/// JSON-RPC method name for proxy initialization.
pub const METHOD_INITIALIZE_PROXY: &str = "proxy/initialize";
