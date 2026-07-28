#![cfg(feature = "unstable_plan_operations")]

use agent_client_protocol::JsonRpcMessage;
use agent_client_protocol::schema::v1::{
    AgentNotification, ClientCapabilities, PlanCapabilities, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, PlanRemoved, PlanUpdate, PlanUpdateContent, SessionNotification,
    SessionUpdate,
};
use serde_json::{Value, json};

fn plan_entry() -> PlanEntry {
    PlanEntry::new(
        "Implement the change",
        PlanEntryPriority::High,
        PlanEntryStatus::InProgress,
    )
}

fn parse_v1_notification(update: SessionUpdate) -> (Value, AgentNotification) {
    let untyped = SessionNotification::new("session-1", update)
        .to_untyped_message()
        .unwrap();
    let parsed = AgentNotification::parse_message("session/update", &untyped.params).unwrap();
    (untyped.params, parsed)
}

#[test]
fn v1_plan_operations_are_capability_gated_and_routed() {
    let capabilities = ClientCapabilities::new().plan(PlanCapabilities::new());
    assert_eq!(
        serde_json::to_value(capabilities).unwrap()["plan"],
        json!({})
    );

    let contents = [
        (
            PlanUpdateContent::items("plan-1", vec![plan_entry()]),
            "items",
        ),
        (
            PlanUpdateContent::file("plan-1", "file:///workspace/PLAN.md"),
            "file",
        ),
        (PlanUpdateContent::markdown("plan-1", "# Plan"), "markdown"),
    ];

    for (content, content_type) in contents {
        let (params, parsed) =
            parse_v1_notification(SessionUpdate::PlanUpdate(PlanUpdate::new(content)));
        assert_eq!(params["update"]["sessionUpdate"], "plan_update");
        assert_eq!(params["update"]["plan"]["type"], content_type);
        assert_eq!(params["update"]["plan"]["planId"], "plan-1");
        assert!(matches!(
            parsed,
            AgentNotification::SessionNotification(SessionNotification {
                update: SessionUpdate::PlanUpdate(_),
                ..
            })
        ));
    }

    let (params, parsed) =
        parse_v1_notification(SessionUpdate::PlanRemoved(PlanRemoved::new("plan-1")));
    assert_eq!(
        params["update"],
        json!({
            "sessionUpdate": "plan_removed",
            "planId": "plan-1"
        })
    );
    assert!(matches!(
        parsed,
        AgentNotification::SessionNotification(SessionNotification {
            update: SessionUpdate::PlanRemoved(_),
            ..
        })
    ));
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn draft_v2_file_and_removal_operations_are_routed() {
    use agent_client_protocol::schema::v2;

    let notification = v2::UpdateSessionNotification::new(
        "session-1",
        v2::SessionUpdate::PlanUpdate(v2::PlanUpdate::new(v2::PlanUpdateContent::file(
            "plan-1",
            "file:///workspace/PLAN.md",
        ))),
    );
    let untyped = notification.to_untyped_message().unwrap();
    assert_eq!(untyped.params["update"]["plan"]["type"], "file");
    let v2::AgentNotification::UpdateSessionNotification(parsed) =
        v2::AgentNotification::parse_message("session/update", &untyped.params).unwrap()
    else {
        panic!("expected a v2 session update notification");
    };
    assert!(matches!(parsed.update, v2::SessionUpdate::PlanUpdate(_)));

    let notification = v2::UpdateSessionNotification::new(
        "session-1",
        v2::SessionUpdate::PlanRemoved(v2::PlanRemoved::new("plan-1")),
    );
    let untyped = notification.to_untyped_message().unwrap();
    assert_eq!(untyped.params["update"]["sessionUpdate"], "plan_removed");
    let v2::AgentNotification::UpdateSessionNotification(parsed) =
        v2::AgentNotification::parse_message("session/update", &untyped.params).unwrap()
    else {
        panic!("expected a v2 session update notification");
    };
    assert!(matches!(parsed.update, v2::SessionUpdate::PlanRemoved(_)));
}
