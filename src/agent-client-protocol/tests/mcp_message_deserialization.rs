//! A shared method name must not conflate JSON-RPC requests and notifications.
#![cfg(feature = "unstable_mcp_over_acp")]

use agent_client_protocol::{JsonRpcMessage, RawJsonRpcMessage, schema::v1};
use serde_json::{Value, json};

fn params() -> Value {
    // Deliberately identical for both message kinds: dispatch must use the outer
    // envelope, not infer a kind from this opaque inner method or requestId.
    json!({"serverId":"server", "requestId":"logical-id", "method":"custom/message"})
}

#[test]
fn mcp_message_kind_is_selected_by_outer_id() {
    let notification = json!({"jsonrpc":"2.0", "method":"mcp/message", "params":params()});
    let parsed: RawJsonRpcMessage = serde_json::from_value(notification.clone()).unwrap();
    let RawJsonRpcMessage::Notification(parsed) = parsed else {
        panic!("nested requestId must not turn a notification into a request");
    };
    assert_eq!(parsed.method.as_ref(), "mcp/message");
    assert_eq!(parsed.params.unwrap().into_value(), params());

    for id in [json!(42), json!("outer-id")] {
        let mut request = notification.clone();
        request["id"] = id.clone();
        let parsed: RawJsonRpcMessage = serde_json::from_value(request).unwrap();
        let RawJsonRpcMessage::Request(parsed) = parsed else {
            panic!("outer id identifies a request");
        };
        assert_eq!(serde_json::to_value(parsed.id).unwrap(), id);
        assert_eq!(parsed.method.as_ref(), "mcp/message");
        assert_eq!(parsed.params.unwrap().into_value(), params());
    }

    for id in [json!(true), json!({}), json!([])] {
        let mut malformed = notification.clone();
        malformed["id"] = id;
        assert!(
            serde_json::from_value::<RawJsonRpcMessage>(malformed).is_err(),
            "an invalid request id must not fall back to notification deserialization"
        );
    }
}

#[test]
fn v1_mcp_method_is_in_separate_request_and_notification_enums() {
    assert!(matches!(
        v1::AgentRequest::parse_message("mcp/message", &params()).unwrap(),
        v1::AgentRequest::MessageMcpRequest(_)
    ));
    assert!(matches!(
        v1::ClientNotification::parse_message("mcp/message", &params()).unwrap(),
        v1::ClientNotification::MessageMcpNotification(_)
    ));
    assert!(v1::ClientRequest::parse_message("mcp/message", &params()).is_err());
    assert!(v1::AgentNotification::parse_message("mcp/message", &params()).is_err());
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn v2_mcp_method_is_in_separate_request_and_notification_enums() {
    use agent_client_protocol::schema::v2;
    assert!(matches!(
        v2::AgentRequest::parse_message("mcp/message", &params()).unwrap(),
        v2::AgentRequest::MessageMcpRequest(_)
    ));
    assert!(matches!(
        v2::ClientNotification::parse_message("mcp/message", &params()).unwrap(),
        v2::ClientNotification::MessageMcpNotification(_)
    ));
    assert!(v2::ClientRequest::parse_message("mcp/message", &params()).is_err());
    assert!(v2::AgentNotification::parse_message("mcp/message", &params()).is_err());
}
