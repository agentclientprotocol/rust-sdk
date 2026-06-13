//! JSON-RPC trait implementations for protocol-level (`$/`-prefixed) messages.

#[cfg(feature = "unstable_cancel_request")]
use crate::schema::{CancelRequestNotification, ProtocolLevelNotification};

#[cfg(feature = "unstable_cancel_request")]
impl_jsonrpc_notification!(CancelRequestNotification, "$/cancel_request");

#[cfg(feature = "unstable_cancel_request")]
impl_jsonrpc_protocol_level_notification_enum!(ProtocolLevelNotification {
    CancelRequestNotification => "$/cancel_request",
});
