use crate::schema::v1::CompleteElicitationNotification;
use crate::schema::v1::SessionNotification;

impl_jsonrpc_notification!(SessionNotification, "session/update");
impl_jsonrpc_notification!(CompleteElicitationNotification, "elicitation/complete");

#[cfg(feature = "unstable_auth_status")]
impl_jsonrpc_notification!(
    crate::schema::v1::AuthStatusUpdateNotification,
    "auth/status_update"
);
