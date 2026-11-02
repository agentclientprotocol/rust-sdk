#![cfg(feature = "schemars")]

use agent_client_protocol::v1;
#[cfg(feature = "unstable_protocol_v2")]
use agent_client_protocol::v2;

#[test]
fn schemars_feature_enables_v1_protocol_schemas() {
    let schema = schemars::schema_for!(v1::schema::InitializeRequest);
    let schema = serde_json::to_value(schema).unwrap();
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["protocolVersion"].is_object());
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn schemars_feature_enables_v2_protocol_schemas() {
    let schema = schemars::schema_for!(v2::schema::InitializeRequest);
    let schema = serde_json::to_value(schema).unwrap();
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["protocolVersion"].is_object());
}
