use jsonrpcmsg::{Message, Params};

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

pub(crate) fn is_initialize_request(msg: &Message) -> bool {
    matches!(msg, Message::Request(req) if req.method == "initialize" && req.id.is_some())
}

pub(crate) fn session_id_from_params(params: &Params) -> Option<String> {
    match params {
        Params::Object(map) => map
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(String::from),
        Params::Array(_) => None,
    }
}
