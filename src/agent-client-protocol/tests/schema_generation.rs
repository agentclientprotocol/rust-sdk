#![cfg(feature = "schemars")]

use agent_client_protocol::schema;

#[test]
fn schemars_feature_enables_v1_protocol_schemas() {
    let schema = schemars::schema_for!(schema::v1::InitializeRequest);
    let schema = serde_json::to_value(schema).unwrap();
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["protocolVersion"].is_object());
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn schemars_feature_enables_v2_protocol_schemas() {
    let schema = schemars::schema_for!(schema::v2::InitializeRequest);
    let schema = serde_json::to_value(schema).unwrap();
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["protocolVersion"].is_object());
}
