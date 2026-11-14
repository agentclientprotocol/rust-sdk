#![cfg(feature = "unstable_plan_operations")]

use agent_client_protocol::JsonRpcMessage;
use agent_client_protocol::v1::schema::{
    AgentNotification, ClientCapabilities, PlanCapabilities, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, PlanMarkdown, PlanRemoved, PlanUpdate, PlanUpdateContent, SessionNotification,
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
        (PlanUpdateContent::markdown("plan-1", "# Plan"), "markdown"),
        (
            PlanUpdateContent::Markdown(
                PlanMarkdown::new("plan-1", "# Plan").path("/workspace/PLAN.md"),
            ),
            "markdown",
        ),
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

/// Schema 1.18 replaced the `file` plan type with an optional `path` on
/// markdown plans.
#[test]
fn v1_markdown_plan_path_names_the_plan_file() {
    let (params, parsed) = parse_v1_notification(SessionUpdate::PlanUpdate(PlanUpdate::new(
        PlanUpdateContent::Markdown(
            PlanMarkdown::new("plan-1", "# Plan").path("/workspace/PLAN.md"),
        ),
    )));
    assert_eq!(
        params["update"]["plan"],
        json!({
            "type": "markdown",
            "planId": "plan-1",
            "content": "# Plan",
            "path": "/workspace/PLAN.md"
        })
    );
    let AgentNotification::SessionNotification(SessionNotification {
        update: SessionUpdate::PlanUpdate(update),
        ..
    }) = parsed
    else {
        panic!("expected a plan update");
    };
    let PlanUpdateContent::Markdown(markdown) = update.plan else {
        panic!("expected a markdown plan");
    };
    assert_eq!(
        markdown.path.as_deref(),
        Some(std::path::Path::new("/workspace/PLAN.md"))
    );

    // `path` is optional: omitted, `null` and a malformed value all mean the
    // plan has no file, and the content still parses.
    for plan in [
        json!({ "type": "markdown", "planId": "plan-1", "content": "# Plan" }),
        json!({ "type": "markdown", "planId": "plan-1", "content": "# Plan", "path": null }),
        json!({ "type": "markdown", "planId": "plan-1", "content": "# Plan", "path": 42 }),
    ] {
        let content: PlanUpdateContent = serde_json::from_value(plan).unwrap();
        assert_eq!(content, PlanUpdateContent::markdown("plan-1", "# Plan"));
    }

    // v1 has no fallback variant, so the removed `file` type no longer parses.
    assert!(
        serde_json::from_value::<PlanUpdateContent>(json!({
            "type": "file",
            "planId": "plan-1",
            "uri": "file:///workspace/PLAN.md"
        }))
        .is_err()
    );
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn draft_v2_markdown_path_and_removal_operations_are_routed() {
    use agent_client_protocol::v2::schema as v2;

    let notification = v2::UpdateSessionNotification::new(
        "session-1",
        v2::SessionUpdate::PlanUpdate(v2::PlanUpdate::new(v2::PlanUpdateContent::Markdown(
            v2::PlanMarkdown::new("plan-1", "# Plan").path("/workspace/PLAN.md"),
        ))),
    );
    let untyped = notification.to_untyped_message().unwrap();
    assert_eq!(
        untyped.params["update"]["plan"],
        json!({
            "type": "markdown",
            "planId": "plan-1",
            "content": "# Plan",
            "path": "/workspace/PLAN.md"
        })
    );
    let v2::AgentNotification::UpdateSessionNotification(parsed) =
        v2::AgentNotification::parse_message("session/update", &untyped.params).unwrap()
    else {
        panic!("expected a v2 session update notification");
    };
    assert!(matches!(
        parsed.update,
        v2::SessionUpdate::PlanUpdate(v2::PlanUpdate {
            plan: v2::PlanUpdateContent::Markdown(_),
            ..
        })
    ));

    // The removed `file` type is an unknown plan type now. v2 keeps its
    // payload for proxying instead of failing the update.
    let file_plan = json!({
        "sessionId": "session-1",
        "update": {
            "sessionUpdate": "plan_update",
            "plan": { "type": "file", "planId": "plan-1", "uri": "file:///workspace/PLAN.md" }
        }
    });
    let parsed = v2::AgentNotification::parse_message("session/update", &file_plan).unwrap();
    let v2::AgentNotification::UpdateSessionNotification(notification) = &parsed else {
        panic!("expected a v2 session update notification");
    };
    assert!(matches!(
        &notification.update,
        v2::SessionUpdate::PlanUpdate(v2::PlanUpdate {
            plan: v2::PlanUpdateContent::Other(_),
            ..
        })
    ));
    assert_eq!(parsed.to_untyped_message().unwrap().params, file_plan);

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
