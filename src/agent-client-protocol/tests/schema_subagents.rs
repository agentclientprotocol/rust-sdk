#![cfg(feature = "unstable_subagents")]

use agent_client_protocol::schema::v1;
use agent_client_protocol::{JsonRpcMessage, JsonRpcNotification};
use serde_json::json;

fn assert_notification<T: JsonRpcNotification>() {}

#[test]
fn v1_subagent_update_uses_existing_session_notification_route() {
    let notification = v1::SessionNotification::new(
        "parent",
        v1::SessionUpdate::SubagentUpdate(v1::SubagentUpdate::new("child")),
    );

    assert_notification::<v1::SessionNotification>();
    let untyped = notification.to_untyped_message().unwrap();
    assert_eq!(untyped.method, "session/update");
    assert_eq!(
        untyped.params,
        json!({
            "sessionId": "parent",
            "update": {
                "sessionUpdate": "subagent_update",
                "sessionId": "child"
            }
        })
    );

    let parsed = v1::AgentNotification::parse_message("session/update", &untyped.params).unwrap();
    let v1::AgentNotification::SessionNotification(parsed) = parsed else {
        panic!("expected session notification");
    };
    assert_eq!(parsed.session_id.0.as_ref(), "parent");
    let v1::SessionUpdate::SubagentUpdate(update) = parsed.update else {
        panic!("expected typed subagent update");
    };
    assert_eq!(update.session_id.0.as_ref(), "child");
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn v2_subagent_update_uses_existing_session_notification_route() {
    use agent_client_protocol::schema::v2;

    let notification = v2::UpdateSessionNotification::new(
        "parent",
        v2::SessionUpdate::SubagentUpdate(v2::SubagentUpdate::new("child")),
    );

    assert_notification::<v2::UpdateSessionNotification>();
    let untyped = notification.to_untyped_message().unwrap();
    assert_eq!(untyped.method, "session/update");
    assert_eq!(
        untyped.params,
        json!({
            "sessionId": "parent",
            "update": {
                "sessionUpdate": "subagent_update",
                "sessionId": "child"
            }
        })
    );

    let parsed = v2::AgentNotification::parse_message("session/update", &untyped.params).unwrap();
    let v2::AgentNotification::UpdateSessionNotification(parsed) = parsed else {
        panic!("expected session notification");
    };
    assert_eq!(parsed.session_id.0.as_ref(), "parent");
    let v2::SessionUpdate::SubagentUpdate(update) = parsed.update else {
        panic!("expected typed subagent update");
    };
    assert_eq!(update.session_id.0.as_ref(), "child");
}
