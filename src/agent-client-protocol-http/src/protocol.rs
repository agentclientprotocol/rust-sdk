use agent_client_protocol::{RawJsonRpcMessage, RawJsonRpcParams};

pub(crate) const HEADER_CONNECTION_ID: &str = "Acp-Connection-Id";
pub(crate) const HEADER_SESSION_ID: &str = "Acp-Session-Id";
pub(crate) const EVENT_STREAM_MIME_TYPE: &str = "text/event-stream";
pub(crate) const JSON_MIME_TYPE: &str = "application/json";

pub(crate) fn method_requires_session_header(method: &str) -> bool {
    matches!(
        method,
        "session/prompt"
            | "session/cancel"
            | "session/load"
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
