use crate::schema::CancelNotification;
use serde::Serialize;

use crate::jsonrpc::{JrMessage, JrNotification};
use crate::util::json_cast;

impl JrMessage for CancelNotification {
    fn into_untyped_message(self) -> Result<crate::UntypedMessage, crate::Error> {
        let method = self.method().to_string();
        crate::UntypedMessage::new(&method, self)
    }

    fn method(&self) -> &str {
        "session/cancel"
    }

    fn parse_request(
        _method: &str,
        _params: &impl Serialize,
    ) -> Option<Result<Self, crate::Error>> {
        // This is a notification, not a request
        None
    }

    fn parse_notification(
        method: &str,
        params: &impl Serialize,
    ) -> Option<Result<Self, crate::Error>> {
        if method != "session/cancel" {
            return None;
        }

        Some(json_cast(params))
    }
}

impl JrNotification for CancelNotification {}
