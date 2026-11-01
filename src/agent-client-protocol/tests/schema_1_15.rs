//! Schema 1.15 methods parse into the SDK's typed v1 and v2 message enums.

#![cfg(any(
    feature = "unstable_auth_status",
    feature = "unstable_session_archive",
    feature = "unstable_session_status",
    feature = "unstable_session_set_title",
    feature = "unstable_protocol_v2"
))]
#![allow(unused_imports)]

use agent_client_protocol::schema::v1;
use agent_client_protocol::{JsonRpcMessage, JsonRpcResponse};
use serde_json::{Value, json};

/// Parse `params` as `method` into the enum `E`, check the parsed method, and
/// check that the message serializes back to the same params.
fn assert_message_round_trips<E: JsonRpcMessage>(method: &str, params: &Value) {
    let parsed = E::parse_message(method, params)
        .unwrap_or_else(|error| panic!("{method} should parse: {error:?}"));
    assert_eq!(parsed.method(), method);
    let untyped = parsed.to_untyped_message().unwrap();
    assert_eq!(untyped.method(), method);
    assert_eq!(untyped.params(), params, "{method} params round trip");
}

/// Parse `result` as the response to `method` into `R` and check that it
/// serializes back to the same JSON.
fn assert_response_round_trips<R: JsonRpcResponse>(method: &str, result: &Value) {
    let parsed = R::from_json(method, result.clone())
        .unwrap_or_else(|error| panic!("{method} response should parse: {error:?}"));
    assert_eq!(
        &parsed.into_json(method).unwrap(),
        result,
        "{method} result"
    );
}

#[cfg(feature = "unstable_auth_status")]
#[test]
fn v1_auth_status_round_trips() {
    assert_message_round_trips::<v1::ClientRequest>("auth/status", &json!({}));
    assert_message_round_trips::<v1::AuthStatusRequest>("auth/status", &json!({}));
    assert_response_round_trips::<v1::AgentResponse>(
        "auth/status",
        &json!({ "state": "authenticated", "accounts": [{ "label": "Key", "active": true }] }),
    );
    assert_response_round_trips::<v1::AuthStatusResponse>(
        "auth/status",
        &json!({ "state": "not_required" }),
    );
    assert_message_round_trips::<v1::AgentNotification>(
        "auth/status_update",
        &json!({ "state": "unauthenticated" }),
    );
    assert_message_round_trips::<v1::AuthStatusUpdateNotification>(
        "auth/status_update",
        &json!({ "state": "unauthenticated" }),
    );
}

#[cfg(feature = "unstable_session_archive")]
#[test]
fn v1_session_archive_round_trips() {
    for method in ["session/archive", "session/unarchive"] {
        let params = json!({ "sessionId": "sess_1" });
        assert_message_round_trips::<v1::ClientRequest>(method, &params);
        assert_response_round_trips::<v1::AgentResponse>(method, &json!({}));
    }
    assert_message_round_trips::<v1::ArchiveSessionRequest>(
        "session/archive",
        &json!({ "sessionId": "sess_1" }),
    );
    assert_message_round_trips::<v1::UnarchiveSessionRequest>(
        "session/unarchive",
        &json!({ "sessionId": "sess_1" }),
    );
}

#[cfg(feature = "unstable_session_status")]
#[test]
fn v1_session_status_round_trips() {
    let params = json!({ "sessionId": "sess_1" });
    assert_message_round_trips::<v1::ClientRequest>("session/status", &params);
    assert_message_round_trips::<v1::SessionStatusRequest>("session/status", &params);
    assert_response_round_trips::<v1::AgentResponse>(
        "session/status",
        &json!({ "status": "saved" }),
    );
    assert_response_round_trips::<v1::SessionStatusResponse>(
        "session/status",
        &json!({ "status": "open" }),
    );
}

#[cfg(feature = "unstable_session_set_title")]
#[test]
fn v1_session_set_title_round_trips() {
    let params = json!({ "sessionId": "sess_1", "title": "Refactor" });
    assert_message_round_trips::<v1::ClientRequest>("session/set_title", &params);
    assert_message_round_trips::<v1::SetSessionTitleRequest>("session/set_title", &params);
    assert_response_round_trips::<v1::AgentResponse>(
        "session/set_title",
        &json!({ "title": "Refactor" }),
    );
}

#[cfg(feature = "unstable_protocol_v2")]
mod v2_methods {
    use agent_client_protocol::JsonRpcMessage;
    use agent_client_protocol::schema::v2;
    use serde_json::json;

    use super::{assert_message_round_trips, assert_response_round_trips};

    #[test]
    fn v2_background_work_cancellation_round_trips() {
        let tool_call = json!({ "sessionId": "sess_1", "toolCallId": "call_1" });
        assert_message_round_trips::<v2::ClientRequest>("tool_call/cancel", &tool_call);
        assert_message_round_trips::<v2::CancelToolCallRequest>("tool_call/cancel", &tool_call);
        assert_response_round_trips::<v2::AgentResponse>("tool_call/cancel", &json!({}));

        let terminal = json!({ "sessionId": "sess_1", "terminalId": "term_1" });
        assert_message_round_trips::<v2::ClientRequest>("terminal/cancel", &terminal);
        assert_message_round_trips::<v2::CancelTerminalRequest>("terminal/cancel", &terminal);
        assert_response_round_trips::<v2::AgentResponse>("terminal/cancel", &json!({}));
    }

    #[cfg(feature = "unstable_auth_status")]
    #[test]
    fn v2_auth_status_round_trips() {
        assert_message_round_trips::<v2::ClientRequest>("auth/status", &json!({}));
        assert_response_round_trips::<v2::AgentResponse>(
            "auth/status",
            &json!({ "state": "unknown" }),
        );
        assert_message_round_trips::<v2::AgentNotification>(
            "auth/status_update",
            &json!({ "state": "authenticated" }),
        );
        assert_message_round_trips::<v2::AuthStatusUpdateNotification>(
            "auth/status_update",
            &json!({ "state": "authenticated" }),
        );
    }

    #[cfg(feature = "unstable_session_archive")]
    #[test]
    fn v2_session_archive_round_trips() {
        for method in ["session/archive", "session/unarchive"] {
            assert_message_round_trips::<v2::ClientRequest>(method, &json!({ "sessionId": "s" }));
            assert_response_round_trips::<v2::AgentResponse>(method, &json!({}));
        }
    }

    #[cfg(feature = "unstable_session_status")]
    #[test]
    fn v2_session_status_round_trips() {
        let params = json!({ "sessionId": "s" });
        assert_message_round_trips::<v2::ClientRequest>("session/status", &params);
        assert_message_round_trips::<v2::SessionStatusRequest>("session/status", &params);
        assert_response_round_trips::<v2::AgentResponse>(
            "session/status",
            &json!({ "status": "open" }),
        );
    }

    #[cfg(feature = "unstable_session_set_title")]
    #[test]
    fn v2_session_set_title_round_trips() {
        let params = json!({ "sessionId": "s", "title": "Refactor" });
        assert_message_round_trips::<v2::ClientRequest>("session/set_title", &params);
        assert_message_round_trips::<v2::SetSessionTitleRequest>("session/set_title", &params);
        assert_response_round_trips::<v2::AgentResponse>(
            "session/set_title",
            &json!({ "title": "Refactor" }),
        );
    }
}
