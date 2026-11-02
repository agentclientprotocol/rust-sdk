use crate::v1::schema::CompleteElicitationNotification;
use crate::v1::schema::SessionNotification;

impl_jsonrpc_notification!(SessionNotification, "session/update");
impl_jsonrpc_notification!(CompleteElicitationNotification, "elicitation/complete");

#[cfg(feature = "unstable_auth_status")]
impl_jsonrpc_notification!(
    crate::v1::schema::AuthStatusUpdateNotification,
    "auth/status_update"
);
