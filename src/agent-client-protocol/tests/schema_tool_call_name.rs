#![cfg(feature = "unstable_tool_call_name")]

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

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn v2_tool_call_name_has_patch_semantics() {
    use agent_client_protocol::schema::{MaybeUndefined, v2::ToolCallUpdate};

    let named = ToolCallUpdate::new("call_1").name("read_file");
    assert_eq!(named.name, MaybeUndefined::Value("read_file".to_string()));

    let omitted = ToolCallUpdate::new("call_1");
    assert_eq!(omitted.name, MaybeUndefined::Undefined);

    let cleared = ToolCallUpdate::new("call_1").name(None::<String>);
    assert_eq!(cleared.name, MaybeUndefined::Null);
}
