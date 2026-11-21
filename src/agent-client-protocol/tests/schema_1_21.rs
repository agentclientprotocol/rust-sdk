//! Checks the SDK's `IndexMap` re-export and the boxed v2 payloads from schema 1.21.1.

use agent_client_protocol::schema::IndexMap;
use agent_client_protocol::v1::schema as v1;
use serde_json::json;

/// An unknown v1 state is built from the re-exported map type and keeps the
/// order of its fields.
#[test]
fn v1_unknown_state_is_built_from_the_reexported_map() {
    let fields = IndexMap::from([
        ("zeta".to_string(), json!(1)),
        ("alpha".to_string(), json!(2)),
    ]);
    let state = v1::StateUpdate::Other(v1::OtherStateUpdate::new("_paused", fields));
    assert_eq!(
        serde_json::to_string(&state).unwrap(),
        r#"{"state":"_paused","zeta":1,"alpha":2}"#
    );
}

/// The re-export is the type the schema's fields use, so a value built with
/// it is accepted where the schema expects one.
#[test]
fn reexported_map_is_the_type_of_the_schema_fields() {
    let state: v1::OtherStateUpdate =
        serde_json::from_str(r#"{"state":"_paused","zeta":1,"alpha":2}"#).unwrap();
    let fields: &IndexMap<String, serde_json::Value> = &state.fields;
    assert_eq!(fields.keys().collect::<Vec<_>>(), ["zeta", "alpha"]);
}

#[cfg(feature = "unstable_protocol_v2")]
mod v2_tests {
    use agent_client_protocol::schema::IndexMap;
    use agent_client_protocol::v2::schema as v2;
    use serde_json::json;

    /// The content of an accepted form and an unknown content block are
    /// built from the re-exported map type.
    #[test]
    fn v2_ordered_maps_are_built_from_the_reexported_map() {
        let block = v2::ContentBlock::Other(v2::OtherContentBlock::new(
            "_custom",
            IndexMap::from([
                ("zeta".to_string(), json!(1)),
                ("alpha".to_string(), json!(2)),
            ]),
        ));
        assert_eq!(
            serde_json::to_string(&block).unwrap(),
            r#"{"type":"_custom","zeta":1,"alpha":2}"#
        );

        let content = IndexMap::from([
            ("zeta".to_string(), v2::ElicitationContentValue::from("z")),
            ("alpha".to_string(), v2::ElicitationContentValue::from(true)),
        ]);
        let action = v2::ElicitationAcceptAction::new().content(content);
        assert_eq!(
            serde_json::to_string(&action).unwrap(),
            r#"{"content":{"zeta":"z","alpha":true}}"#
        );
    }

    #[test]
    fn custom_config_value_converts_to_a_boxed_variant() {
        let other =
            v2::OtherSessionConfigOptionValue::new("_text", json!("hello"), IndexMap::new());
        let value = v2::SessionConfigOptionValue::from(other.clone());
        assert_eq!(
            serde_json::to_value(&value).unwrap(),
            json!({"type": "_text", "value": "hello"})
        );
        let v2::SessionConfigOptionValue::Other(boxed) = value else {
            panic!("expected a custom config value");
        };
        assert_eq!(*boxed, other);
    }

    #[test]
    fn diff_converts_to_boxed_tool_call_content() {
        let diff = v2::Diff::new(vec![v2::DiffChange::modify("/repo/file.txt")]);
        let content = v2::ToolCallContent::from(diff.clone());
        assert_eq!(
            serde_json::to_value(&content).unwrap(),
            json!({"type": "diff", "changes": [{"operation": "modify", "path": "/repo/file.txt"}]})
        );
        let v2::ToolCallContent::Diff(boxed) = content else {
            panic!("expected diff content");
        };
        assert_eq!(*boxed, diff);
    }
}
