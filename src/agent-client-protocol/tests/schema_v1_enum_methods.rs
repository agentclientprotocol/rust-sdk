//! Method names reported by the v1 schema dispatch enums.

use agent_client_protocol::{Error, ErrorCode, JsonRpcMessage, v1::schema as v1};
use serde_json::json;

#[test]
fn v1_extension_enum_parsing_preserves_method_prefix() -> Result<(), Error> {
    let params = json!({ "payload": true });

    let request = v1::ClientRequest::parse_message("_vendor/request", &params)?;
    assert_eq!(request.method(), "_vendor/request");
    let untyped_request = request.to_untyped_message()?;
    assert_eq!(untyped_request.method(), "_vendor/request");
    assert_eq!(untyped_request.params(), &params);

    let request = v1::AgentRequest::parse_message("_vendor/request", &params)?;
    assert_eq!(request.method(), "_vendor/request");

    let notification = v1::AgentNotification::parse_message("_vendor/notify", &params)?;
    assert_eq!(notification.method(), "_vendor/notify");
    let untyped_notification = notification.to_untyped_message()?;
    assert_eq!(untyped_notification.method(), "_vendor/notify");
    assert_eq!(untyped_notification.params(), &params);

    let notification = v1::ClientNotification::parse_message("_vendor/notify", &params)?;
    assert_eq!(notification.method(), "_vendor/notify");

    Ok(())
}

#[test]
fn v1_enum_parsing_rejects_unknown_non_extension_methods() {
    let error = v1::ClientRequest::parse_message("vendor/request", &json!({})).unwrap_err();
    assert_eq!(error.code, ErrorCode::METHOD_NOT_FOUND);
}

#[test]
fn v1_enum_method_matches_schema_method_names() -> Result<(), Error> {
    let request = v1::ClientRequest::PromptRequest(v1::PromptRequest::new(
        v1::SessionId::new("session-1"),
        vec![],
    ));
    assert_eq!(JsonRpcMessage::method(&request), request.method());
    assert_eq!(request.to_untyped_message()?.method(), "session/prompt");
    Ok(())
}
