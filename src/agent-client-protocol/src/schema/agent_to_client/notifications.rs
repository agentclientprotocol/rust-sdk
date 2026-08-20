use crate::schema::v1::CompleteElicitationNotification;
use crate::schema::v1::SessionNotification;

impl_jsonrpc_notification!(SessionNotification, "session/update");
impl_jsonrpc_notification!(CompleteElicitationNotification, "elicitation/complete");
