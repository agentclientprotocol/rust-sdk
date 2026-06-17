use agent_client_protocol::{RawJsonRpcMessage, RawJsonRpcParams};

pub(crate) const HEADER_CONNECTION_ID: &str = "acp-connection-id";
pub(crate) const HEADER_SESSION_ID: &str = "acp-session-id";
pub(crate) const EVENT_STREAM_MIME_TYPE: &str = "text/event-stream";
pub(crate) const JSON_MIME_TYPE: &str = "application/json";

pub(crate) fn method_requires_session_header(method: &str) -> bool {
    matches!(
        method,
        "session/prompt"
            | "session/cancel"
            | "session/close"
            | "session/delete"
            | "session/load"
            | "session/resume"
            | "session/set_config_option"
            | "session/set_mode"
            | "session/set_model"
    )
}

pub(crate) fn is_initialize_request(msg: &RawJsonRpcMessage) -> bool {
    matches!(msg, RawJsonRpcMessage::Request(req) if req.method.as_ref() == "initialize")
}

pub(crate) fn method_for_message(msg: &RawJsonRpcMessage) -> Option<&str> {
    match msg {
        RawJsonRpcMessage::Request(req) => Some(req.method.as_ref()),
        RawJsonRpcMessage::Notification(notification) => Some(notification.method.as_ref()),
        RawJsonRpcMessage::Response(_) => None,
    }
}

pub(crate) fn session_id_from_params(params: &RawJsonRpcParams) -> Option<String> {
    match params {
        RawJsonRpcParams::Object(map) => map
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(String::from),
        RawJsonRpcParams::Array(_) => None,
    }
}

pub(crate) fn session_id_from_message(msg: &RawJsonRpcMessage) -> Option<String> {
    match msg {
        RawJsonRpcMessage::Request(req) => req.params.as_ref().and_then(session_id_from_params),
        RawJsonRpcMessage::Notification(notification) => notification
            .params
            .as_ref()
            .and_then(session_id_from_params),
        RawJsonRpcMessage::Response(_) => None,
    }
}

pub(crate) fn apply_session_header_to_message(
    msg: &mut RawJsonRpcMessage,
    session_id: &str,
) -> Result<(), &'static str> {
    match msg {
        RawJsonRpcMessage::Request(req) => {
            apply_session_header_to_params(&mut req.params, session_id)
        }
        RawJsonRpcMessage::Notification(notification) => {
            apply_session_header_to_params(&mut notification.params, session_id)
        }
        RawJsonRpcMessage::Response(_) => Ok(()),
    }
}

fn apply_session_header_to_params(
    params: &mut Option<RawJsonRpcParams>,
    session_id: &str,
) -> Result<(), &'static str> {
    match params {
        Some(RawJsonRpcParams::Object(map)) => {
            match map.get("sessionId") {
                Some(serde_json::Value::String(existing)) if existing == session_id => {}
                Some(serde_json::Value::String(_)) => {
                    return Err("Acp-Session-Id header does not match params.sessionId");
                }
                Some(_) => return Err("params.sessionId must be a string"),
                None => {
                    map.insert(
                        "sessionId".to_string(),
                        serde_json::Value::String(session_id.to_string()),
                    );
                }
            }
            Ok(())
        }
        Some(RawJsonRpcParams::Array(_)) => Err("Acp-Session-Id header requires object params"),
        None => {
            let mut map = serde_json::Map::new();
            map.insert(
                "sessionId".to_string(),
                serde_json::Value::String(session_id.to_string()),
            );
            *params = Some(RawJsonRpcParams::Object(map));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::RequestId;
    use axum::http::{HeaderMap, HeaderValue};
    use serde_json::json;

    use super::*;

    #[test]
    fn custom_response_headers_are_valid_static_header_names() {
        let mut headers = HeaderMap::new();

        headers.insert(HEADER_CONNECTION_ID, HeaderValue::from_static("conn-1"));
        headers.insert(HEADER_SESSION_ID, HeaderValue::from_static("session-1"));

        assert_eq!(headers[HEADER_CONNECTION_ID], "conn-1");
        assert_eq!(headers[HEADER_SESSION_ID], "session-1");
    }

    #[test]
    fn session_header_is_inserted_into_object_params() {
        let mut message = RawJsonRpcMessage::request(
            "session/prompt".to_string(),
            json!({ "prompt": [] }),
            RequestId::Number(1),
        )
        .unwrap();

        apply_session_header_to_message(&mut message, "session-1").unwrap();

        assert_eq!(
            session_id_from_message(&message).as_deref(),
            Some("session-1")
        );
    }

    #[test]
    fn session_header_conflict_is_rejected() {
        let mut message = RawJsonRpcMessage::request(
            "session/prompt".to_string(),
            json!({ "sessionId": "session-1" }),
            RequestId::Number(1),
        )
        .unwrap();

        let error = apply_session_header_to_message(&mut message, "session-2").unwrap_err();

        assert_eq!(
            error,
            "Acp-Session-Id header does not match params.sessionId"
        );
    }

    #[test]
    fn all_session_scoped_client_methods_require_session_header() {
        for method in [
            "session/cancel",
            "session/close",
            "session/delete",
            "session/load",
            "session/prompt",
            "session/resume",
            "session/set_config_option",
            "session/set_mode",
            "session/set_model",
        ] {
            assert!(
                method_requires_session_header(method),
                "{method} should require Acp-Session-Id or params.sessionId"
            );
        }
    }
}
