use agent_client_protocol::JsonRpcMessage;
use agent_client_protocol::schema::v1::{ToolCall, ToolCallUpdateFields};
use serde_json::json;

#[test]
fn v1_tool_call_name_serializes_and_updates() {
    let mut tool_call = ToolCall::new("call_1", "Read configuration").name("read_file");

    assert_eq!(
        serde_json::to_value(&tool_call).unwrap()["name"],
        json!("read_file")
    );

    tool_call.update(ToolCallUpdateFields::new().name("write_file"));
    assert_eq!(tool_call.name.as_deref(), Some("write_file"));

    tool_call.update(ToolCallUpdateFields::new());
    assert_eq!(tool_call.name.as_deref(), Some("write_file"));

    let null_name: ToolCallUpdateFields = serde_json::from_value(json!({ "name": null })).unwrap();
    tool_call.update(null_name);
    assert_eq!(tool_call.name.as_deref(), Some("write_file"));
}

#[test]
fn v1_tool_call_names_survive_session_notifications() {
    use agent_client_protocol::schema::v1::{
        AgentNotification, SessionNotification, SessionUpdate, ToolCallUpdate,
    };

    let tool_call = ToolCall::new("call_1", "Read configuration").name("read_file");
    for update in [
        SessionUpdate::ToolCall(tool_call.clone()),
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::from(tool_call)),
    ] {
        let notification = SessionNotification::new("session-1", update);
        let untyped = notification.to_untyped_message().unwrap();
        assert_eq!(untyped.method, "session/update");
        assert_eq!(untyped.params["update"]["name"], json!("read_file"));

        let parsed = AgentNotification::parse_message("session/update", &untyped.params).unwrap();
        assert_eq!(parsed.to_untyped_message().unwrap().params, untyped.params);
    }
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn v2_tool_call_name_has_patch_semantics() {
    use agent_client_protocol::schema::{MaybeUndefined, v2::ToolCallUpdate};

    let named = ToolCallUpdate::new("call_1").name("read_file");
    assert_eq!(named.name, MaybeUndefined::Value("read_file".to_string()));
    assert_eq!(
        serde_json::to_value(&named).unwrap(),
        json!({ "toolCallId": "call_1", "name": "read_file" })
    );

    let omitted = ToolCallUpdate::new("call_1");
    assert_eq!(omitted.name, MaybeUndefined::Undefined);
    assert_eq!(
        serde_json::to_value(&omitted).unwrap(),
        json!({ "toolCallId": "call_1" })
    );

    let cleared = ToolCallUpdate::new("call_1").name(None::<String>);
    assert_eq!(cleared.name, MaybeUndefined::Null);
    assert_eq!(
        serde_json::to_value(&cleared).unwrap(),
        json!({ "toolCallId": "call_1", "name": null })
    );

    let mut stored = named;
    stored.apply_update(omitted);
    assert_eq!(stored.name, MaybeUndefined::Value("read_file".to_string()));
    stored.apply_update(cleared);
    assert_eq!(stored.name, MaybeUndefined::Null);
    stored.apply_update(ToolCallUpdate::new("call_1").name("write_file"));
    assert_eq!(stored.name, MaybeUndefined::Value("write_file".to_string()));
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn v2_tool_call_names_survive_session_notifications() {
    use agent_client_protocol::schema::v2::{
        AgentNotification, SessionUpdate, ToolCallUpdate, UpdateSessionNotification,
    };

    for tool_call in [
        ToolCallUpdate::new("call_1").name("read_file"),
        ToolCallUpdate::new("call_1"),
        ToolCallUpdate::new("call_1").name(None::<String>),
    ] {
        let notification =
            UpdateSessionNotification::new("session-1", SessionUpdate::ToolCallUpdate(tool_call));
        let untyped = notification.to_untyped_message().unwrap();
        assert_eq!(untyped.method, "session/update");

        let parsed = AgentNotification::parse_message("session/update", &untyped.params).unwrap();
        assert_eq!(parsed.to_untyped_message().unwrap().params, untyped.params);
    }
}
