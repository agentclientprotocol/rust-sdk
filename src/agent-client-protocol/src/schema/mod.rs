//! Version-neutral ACP schema items.
//!
//! Each protocol version's messages (requests, responses, notifications and
//! supporting types) are in that version's SDK module: [`crate::v1::schema`]
//! and, with `unstable_protocol_v2`, `agent_client_protocol::v2::schema`. This
//! module holds only the items every version shares: [`ProtocolVersion`],
//! [`MaybeUndefined`], [`MethodDispatchError`], the [`IntoOption`] and
//! [`IntoMaybeUndefined`] conversion traits, and [`SuccessorMessage`].
//!
//! The types come from the `agent-client-protocol-schema` crate, which this
//! crate pins to one exact version. Use them through the SDK instead of
//! depending on `agent-client-protocol-schema` directly.

// ---------------------------------------------------------------------------
// Macros for implementing JsonRpc traits on schema types
// ---------------------------------------------------------------------------

/// Implement `JsonRpcMessage`, `JsonRpcRequest`, and `JsonRpcResponse` for a
/// request/response pair from the schema crate.
///
/// ```ignore
/// impl_jsonrpc_request!(PromptRequest, PromptResponse, "session/prompt");
/// ```
macro_rules! impl_jsonrpc_request {
    ($req:ty, $resp:ty, $method:literal) => {
        impl $crate::JsonRpcMessage for $req {
            fn matches_method(method: &str) -> bool {
                method == $method
            }

            fn method(&self) -> &str {
                $method
            }

            fn to_untyped_message(&self) -> Result<$crate::UntypedMessage, $crate::Error> {
                $crate::UntypedMessage::new($method, self)
            }

            fn parse_message(
                method: &str,
                params: &serde_json::Value,
            ) -> Result<Self, $crate::Error> {
                if method != $method {
                    return Err($crate::Error::method_not_found());
                }
                $crate::util::json_cast_params(params)
            }
        }

        impl $crate::JsonRpcRequest for $req {
            type Response = $resp;
        }

        impl $crate::JsonRpcResponse for $resp {
            fn into_json(self, _method: &str) -> Result<serde_json::Value, $crate::Error> {
                serde_json::to_value(self).map_err($crate::Error::into_internal_error)
            }

            fn from_json(_method: &str, value: serde_json::Value) -> Result<Self, $crate::Error> {
                $crate::util::json_cast(&value)
            }
        }
    };
}

/// Implement `JsonRpcMessage` and `JsonRpcNotification` for a notification type
/// from the schema crate.
///
/// ```ignore
/// impl_jsonrpc_notification!(CancelNotification, "session/cancel");
/// ```
macro_rules! impl_jsonrpc_notification {
    ($notif:ty, $method:literal) => {
        impl $crate::JsonRpcMessage for $notif {
            fn matches_method(method: &str) -> bool {
                method == $method
            }

            fn method(&self) -> &str {
                $method
            }

            fn to_untyped_message(&self) -> Result<$crate::UntypedMessage, $crate::Error> {
                $crate::UntypedMessage::new($method, self)
            }

            fn parse_message(
                method: &str,
                params: &serde_json::Value,
            ) -> Result<Self, $crate::Error> {
                if method != $method {
                    return Err($crate::Error::method_not_found());
                }
                $crate::util::json_cast_params(params)
            }
        }

        impl $crate::JsonRpcNotification for $notif {}
    };
}

// The schema owns method-to-variant dispatch. These adapters retain SDK handler
// matching, untyped envelopes, and error policy.
macro_rules! impl_jsonrpc_message_enum {
    ($enum:ty, $matches:expr) => {
        impl $crate::JsonRpcMessage for $enum {
            fn matches_method(method: &str) -> bool {
                ($matches)(method)
            }

            fn method(&self) -> &str {
                <$enum>::method(self)
            }

            fn to_untyped_message(&self) -> Result<$crate::UntypedMessage, $crate::Error> {
                $crate::UntypedMessage::new(<$enum>::method(self), self)
            }

            fn parse_message(
                method: &str,
                params: &serde_json::Value,
            ) -> Result<Self, $crate::Error> {
                <$enum>::from_method_and_params(method, params).map_err(|error| {
                    $crate::schema::dispatch_error(
                        error,
                        params.clone(),
                        $crate::Error::invalid_params,
                    )
                })
            }
        }
    };
}

macro_rules! impl_jsonrpc_request_enum {
    ($enum:ty) => {
        // Aggregate request handlers remain catch-all handlers. Their parser
        // rejects unknown methods rather than falling through to another handler.
        impl_jsonrpc_message_enum!($enum, |_| true);
        impl $crate::JsonRpcRequest for $enum {
            type Response = serde_json::Value;
        }
    };
}

macro_rules! impl_jsonrpc_notification_enum {
    ($enum:ty) => {
        impl_jsonrpc_message_enum!($enum, |_| true);
        impl $crate::JsonRpcNotification for $enum {}
    };
}

macro_rules! impl_jsonrpc_protocol_level_notification_enum {
    ($enum:ty) => {
        impl_jsonrpc_message_enum!($enum, <$enum>::supports_method);
        impl $crate::JsonRpcNotification for $enum {}
    };
}

macro_rules! impl_jsonrpc_response_enum {
    ($enum:ty) => {
        impl $crate::JsonRpcResponse for $enum {
            fn into_json(self, _method: &str) -> Result<serde_json::Value, $crate::Error> {
                serde_json::to_value(self).map_err($crate::Error::into_internal_error)
            }

            fn from_json(method: &str, value: serde_json::Value) -> Result<Self, $crate::Error> {
                <$enum>::from_method_and_result(method, value.clone()).map_err(|error| {
                    $crate::schema::dispatch_error(error, value, $crate::Error::parse_error)
                })
            }
        }
    };
}

// Match json_cast_params/json_cast diagnostics without putting SDK error codes
// or payload-bearing diagnostics into the schema's dispatch implementation.
fn dispatch_error(
    error: agent_client_protocol_schema::MethodDispatchError,
    payload: serde_json::Value,
    invalid_payload: fn() -> crate::Error,
) -> crate::Error {
    use agent_client_protocol_schema::MethodDispatchError;

    match error {
        MethodDispatchError::UnknownMethod { .. } => crate::Error::method_not_found(),
        MethodDispatchError::InvalidPayload { source, .. } => {
            invalid_payload().data(serde_json::json!({
                "error": source.to_string(),
                "json": payload,
                "phase": "deserialization",
            }))
        }
        _ => crate::Error::internal_error(),
    }
}

// Internal organization
mod agent_to_client;
mod client_to_agent;
mod enum_impls;
#[cfg(feature = "unstable_mcp_over_acp")]
mod mcp;
mod protocol_level;
mod proxy_protocol;
#[cfg(feature = "unstable_protocol_v2")]
mod v2_impls;

pub use agent_client_protocol_schema::{
    IntoMaybeUndefined, IntoOption, MaybeUndefined, MethodDispatchError, ProtocolVersion,
};

pub use proxy_protocol::SuccessorMessage;
