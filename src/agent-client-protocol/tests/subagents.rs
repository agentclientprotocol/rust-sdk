#![cfg(feature = "unstable_subagents")]

use agent_client_protocol::{
    JsonRpcMessage,
    schema::v1::{SessionNotification, SessionUpdate},
};
use serde_json::json;

#[test]
fn preview_messages_round_trip_through_v1_notification_handlers() {
    for update in [
        json!({
            "sessionUpdate": "session_message", "messageId": "message",
            "senderSessionId": "parent", "recipientSessionId": "child",
            "content": null, "_meta": null
        }),
        json!({
            "sessionUpdate": "session_message_chunk", "messageId": "message",
            "senderSessionId": "parent", "recipientSessionId": "child",
            "content": {"type": "text", "text": "hello"}
        }),
    ] {
        let wire = json!({"sessionId": "parent", "update": update});
        let notification = SessionNotification::parse_message("session/update", &wire).unwrap();
        assert!(matches!(
            notification.update,
            SessionUpdate::SessionMessage(_) | SessionUpdate::SessionMessageChunk(_)
        ));
        assert_eq!(notification.to_untyped_message().unwrap().params, wire);
    }
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn preview_messages_round_trip_through_v2_notification_handlers() {
    use agent_client_protocol::schema::v2::{SessionUpdate, UpdateSessionNotification};

    let wire = json!({"sessionId": "child", "update": {
        "sessionUpdate": "session_message", "messageId": "received",
        "senderSessionId": "parent", "recipientSessionId": "child",
        "content": [{"type": "text", "text": "hello"}],
        "_meta": {"custom": true}
    }});
    let notification = UpdateSessionNotification::parse_message("session/update", &wire).unwrap();
    assert!(matches!(
        notification.update,
        SessionUpdate::SessionMessage(_)
    ));
    assert_eq!(notification.to_untyped_message().unwrap().params, wire);
}
