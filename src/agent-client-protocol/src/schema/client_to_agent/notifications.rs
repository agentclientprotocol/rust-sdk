use crate::v1::schema::CancelNotification;

impl_jsonrpc_notification!(CancelNotification, "session/cancel");
