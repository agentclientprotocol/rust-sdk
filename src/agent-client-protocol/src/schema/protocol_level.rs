#[cfg(feature = "unstable_cancel_request")]
use crate::{
    JsonRpcMessage, JsonRpcNotification, UntypedMessage,
    schema::{CancelRequestNotification, ProtocolLevelNotification},
};

#[cfg(feature = "unstable_cancel_request")]
impl_jsonrpc_notification!(CancelRequestNotification, "$/cancel_request");

#[cfg(feature = "unstable_cancel_request")]
impl JsonRpcMessage for ProtocolLevelNotification {
    fn matches_method(method: &str) -> bool {
        method == "$/cancel_request"
    }

    fn method(&self) -> &str {
        match self {
            Self::CancelRequestNotification(_) => "$/cancel_request",
            _ => "_unknown",
        }
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
        UntypedMessage::new(self.method(), self)
    }

    fn parse_message(method: &str, params: &impl serde::Serialize) -> Result<Self, crate::Error> {
        match method {
            "$/cancel_request" => {
                crate::util::json_cast_params(params).map(Self::CancelRequestNotification)
            }
            _ => Err(crate::Error::method_not_found()),
        }
    }
}

#[cfg(feature = "unstable_cancel_request")]
impl JsonRpcNotification for ProtocolLevelNotification {}
