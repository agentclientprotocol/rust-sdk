#![cfg(feature = "unstable_session_compaction")]

use agent_client_protocol::JsonRpcMessage;
use agent_client_protocol::schema::v1::{
    AgentNotification, ClientCapabilities, ClientSessionCapabilities, CompactionCapabilities,
    CompactionStatus, CompactionSummaryChunk, CompactionUpdate, ContentBlock, SessionNotification,
    SessionUpdate, TextContent,
};
use serde_json::json;

#[test]
fn v1_compaction_capability_and_updates_pass_through_session_notifications() {
    let capabilities = ClientCapabilities::new()
        .session(ClientSessionCapabilities::new().compaction(CompactionCapabilities::new()));
    assert_eq!(
        serde_json::to_value(capabilities).unwrap()["session"]["compaction"],
        json!({})
    );

    let notification = SessionNotification::new(
        "session-1",
        SessionUpdate::CompactionUpdate(CompactionUpdate::new(
            "compaction-1",
            CompactionStatus::InProgress,
        )),
    );
    let untyped = notification.to_untyped_message().unwrap();
    assert_eq!(untyped.method, "session/update");
    assert_eq!(
        untyped.params["update"],
        json!({
            "sessionUpdate": "compaction_update",
            "compactionId": "compaction-1",
            "status": "in_progress"
        })
    );

    let parsed = AgentNotification::parse_message("session/update", &untyped.params).unwrap();
    assert!(matches!(
        parsed,
        AgentNotification::SessionNotification(SessionNotification {
            update: SessionUpdate::CompactionUpdate(_),
            ..
        })
    ));

    let notification = SessionNotification::new(
        "session-1",
        SessionUpdate::CompactionSummaryChunk(CompactionSummaryChunk::new(
            "compaction-1",
            ContentBlock::Text(TextContent::new("retained context")),
        )),
    );
    let untyped = notification.to_untyped_message().unwrap();
    assert_eq!(
        untyped.params["update"],
        json!({
            "sessionUpdate": "compaction_summary_chunk",
            "compactionId": "compaction-1",
            "content": { "type": "text", "text": "retained context" }
        })
    );
    let parsed = AgentNotification::parse_message("session/update", &untyped.params).unwrap();
    assert!(matches!(
        parsed,
        AgentNotification::SessionNotification(SessionNotification {
            update: SessionUpdate::CompactionSummaryChunk(_),
            ..
        })
    ));
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn draft_v2_compaction_updates_pass_through_session_notifications() {
    use agent_client_protocol::schema::v2;

    let notification = v2::UpdateSessionNotification::new(
        "session-1",
        v2::SessionUpdate::CompactionUpdate(v2::CompactionUpdate::new(
            "compaction-1",
            v2::CompactionStatus::Completed,
        )),
    );
    let untyped = notification.to_untyped_message().unwrap();
    assert_eq!(untyped.method, "session/update");
    assert_eq!(
        untyped.params["update"],
        json!({
            "sessionUpdate": "compaction_update",
            "compactionId": "compaction-1",
            "status": "completed"
        })
    );

    let parsed = v2::AgentNotification::parse_message("session/update", &untyped.params).unwrap();
    let v2::AgentNotification::UpdateSessionNotification(parsed) = parsed else {
        panic!("expected a v2 session update notification");
    };
    assert!(matches!(
        parsed.update,
        v2::SessionUpdate::CompactionUpdate(_)
    ));

    let notification = v2::UpdateSessionNotification::new(
        "session-1",
        v2::SessionUpdate::CompactionSummaryChunk(v2::CompactionSummaryChunk::new(
            "compaction-1",
            v2::ContentBlock::Text(v2::TextContent::new("retained context")),
        )),
    );
    let untyped = notification.to_untyped_message().unwrap();
    assert_eq!(
        untyped.params["update"],
        json!({
            "sessionUpdate": "compaction_summary_chunk",
            "compactionId": "compaction-1",
            "content": { "type": "text", "text": "retained context" }
        })
    );
    let parsed = v2::AgentNotification::parse_message("session/update", &untyped.params).unwrap();
    let v2::AgentNotification::UpdateSessionNotification(parsed) = parsed else {
        panic!("expected a v2 session update notification");
    };
    assert!(matches!(
        parsed.update,
        v2::SessionUpdate::CompactionSummaryChunk(_)
    ));
}
