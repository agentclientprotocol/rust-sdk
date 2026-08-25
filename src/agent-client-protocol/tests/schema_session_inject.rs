#![cfg(feature = "unstable_session_inject")]

use agent_client_protocol::{JsonRpcMessage, JsonRpcResponse, schema::v2};
use serde_json::json;

#[test]
fn v2_session_inject_requests_serialize_and_dispatch() {
    let content = vec![v2::ContentBlock::Text(v2::TextContent::new("steer now"))];
    let inject =
        v2::InjectSessionRequest::new("session-1", v2::SessionInjectMode::Steer, content.clone());
    let untyped = inject.to_untyped_message().unwrap();
    assert_eq!(untyped.method, "session/inject");
    assert_eq!(
        untyped.params,
        json!({
            "sessionId": "session-1",
            "mode": "steer",
            "content": [{ "type": "text", "text": "steer now" }]
        })
    );
    assert!(matches!(
        v2::ClientRequest::parse_message(untyped.method(), untyped.params()).unwrap(),
        v2::ClientRequest::InjectSessionRequest(_)
    ));
    assert!(matches!(
        v2::AgentResponse::from_value(
            "session/inject",
            json!({ "messageId": "message-1" }),
        )
        .unwrap(),
        v2::AgentResponse::InjectSessionResponse(response)
            if response.message_id == v2::MessageId::new("message-1")
    ));

    let revoke = v2::RevokeInjectSessionRequest::new("session-1", v2::MessageId::new("message-1"));
    assert_eq!(
        revoke.to_untyped_message().unwrap().params,
        json!({ "sessionId": "session-1", "messageId": "message-1" })
    );
    assert!(matches!(
        v2::ClientRequest::parse_message(
            "session/revoke_inject",
            &json!({ "sessionId": "session-1", "messageId": "message-1" }),
        )
        .unwrap(),
        v2::ClientRequest::RevokeInjectSessionRequest(_)
    ));
    assert!(matches!(
        v2::AgentResponse::from_value("session/revoke_inject", json!({})).unwrap(),
        v2::AgentResponse::RevokeInjectSessionResponse(_)
    ));

    let replace =
        v2::ReplaceInjectSessionRequest::new("session-1", v2::MessageId::new("message-1"), content);
    assert_eq!(
        replace.to_untyped_message().unwrap().params,
        json!({
            "sessionId": "session-1",
            "messageId": "message-1",
            "content": [{ "type": "text", "text": "steer now" }]
        })
    );
    assert!(matches!(
        v2::ClientRequest::parse_message(
            "session/replace_inject",
            &json!({
                "sessionId": "session-1",
                "messageId": "message-1",
                "content": [{ "type": "text", "text": "steer now" }]
            }),
        )
        .unwrap(),
        v2::ClientRequest::ReplaceInjectSessionRequest(_)
    ));
    assert!(matches!(
        v2::AgentResponse::from_value(
            "session/replace_inject",
            json!({ "messageId": "message-1" }),
        )
        .unwrap(),
        v2::AgentResponse::ReplaceInjectSessionResponse(response)
            if response.message_id == v2::MessageId::new("message-1")
    ));
}
