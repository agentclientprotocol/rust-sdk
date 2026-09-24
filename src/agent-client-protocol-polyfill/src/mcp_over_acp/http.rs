//! MCP 2026-07-28 request-scoped Streamable HTTP endpoint.
//!
//! The adapter keeps raw MCP envelopes, ACP cancellation, and response-stream
//! lifetimes explicit. Each POST owns one operation, not an MCP session.

use std::{convert::Infallible, sync::Arc};

use agent_client_protocol::Error;
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::post,
};
use base64::Engine as _;
use futures::{SinkExt, channel::mpsc};
use serde_json::{Map, Value};
use tokio::{
    net::TcpListener,
    sync::{mpsc as tokio_mpsc, oneshot},
};

use super::BridgeMessage;

const VERSION: &str = "2026-07-28";

struct BridgeState {
    server_id: String,
    token: String,
    tx: mpsc::Sender<BridgeMessage>,
}

pub(super) async fn run_http_listener(
    listener: TcpListener,
    server_id: String,
    token: String,
    tx: mpsc::Sender<BridgeMessage>,
) -> Result<(), Error> {
    let state = Arc::new(BridgeState {
        server_id,
        token,
        tx,
    });
    let app = Router::new()
        .route("/", post(handle_post))
        .with_state(state);
    axum::serve(listener, app)
        .await
        .map_err(Error::into_internal_error)
}

fn error(status: StatusCode, id: Value, code: i64, message: &str) -> Response {
    (status, Json(rpc_error(id, code, message))).into_response()
}

pub(super) fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    let mut response =
        serde_json::json!({"jsonrpc":"2.0", "error":{"code":code,"message":message}});
    if !id.is_null() {
        response["id"] = id;
    }
    response
}

fn valid_request_id(id: &Value) -> bool {
    id.is_string() || id.as_i64().is_some() || id.as_u64().is_some()
}

/// Only the MCP 2026 payload metadata carries a subscription identifier.
/// Other fields (including opaque requestState and progress tokens) are untouched.
pub(super) fn rewrite_subscription_id(payload: &mut Value, request_id: &str, http_id: &Value) {
    if let Some(subscription_id) = payload
        .get_mut("_meta")
        .and_then(Value::as_object_mut)
        .and_then(|meta| meta.get_mut("io.modelcontextprotocol/subscriptionId"))
        && subscription_id.as_str() == Some(request_id)
    {
        *subscription_id = http_id.clone();
    }
}

pub(super) fn rpc_result(id: Value, request_id: &str, mut result: Value) -> Value {
    rewrite_subscription_id(&mut result, request_id, &id);
    serde_json::json!({"jsonrpc":"2.0", "id":id, "result":result})
}

pub(super) fn rpc_acp_error(id: Value, error: Error) -> Value {
    serde_json::json!({"jsonrpc":"2.0", "id":id, "error":error})
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn valid_origin(headers: &HeaderMap) -> bool {
    // Browsers supply Origin; only the actual loopback origin is trusted.
    // Non-browser HTTP clients normally omit Origin.
    let Some(origin) = header_value(headers, "origin") else {
        return !headers.contains_key("origin");
    };
    let Some(host) = header_value(headers, "host") else {
        return false;
    };
    host.split_once(':')
        .is_some_and(|(address, port)| address == "127.0.0.1" && port.parse::<u16>().is_ok())
        && origin == format!("http://{host}")
}

fn accepts_both(headers: &HeaderMap) -> bool {
    let Some(accept) = header_value(headers, "accept") else {
        return false;
    };
    let types = accept
        .split(',')
        .map(|part| part.split(';').next().unwrap_or("").trim());
    let types: Vec<_> = types.collect();
    types.contains(&"application/json") && types.contains(&"text/event-stream")
}

fn mirrored_name<'a>(method: &str, params: &'a Map<String, Value>) -> Option<&'a str> {
    match method {
        "tools/call" | "prompts/get" => params.get("name").and_then(Value::as_str),
        "resources/read" => params.get("uri").and_then(Value::as_str),
        _ => None,
    }
}

/// Decode the MCP sentinel; rejecting invalid or noncanonical Base64 prevents
/// intermediaries and the adapter from disagreeing on mirrored routing values.
fn matches_mirror(header: Option<&str>, body: &str) -> bool {
    let Some(header) = header else {
        return false;
    };
    if let Some(encoded) = header
        .strip_prefix("=?base64?")
        .and_then(|h| h.strip_suffix("?="))
    {
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .is_ok_and(|bytes| bytes == body.as_bytes())
    } else {
        // Literal sentinel-looking values must be encoded to avoid ambiguity.
        !header.starts_with("=?base64?") && header == body
    }
}

async fn handle_post(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if [
        "origin",
        "authorization",
        "mcp-protocol-version",
        "mcp-method",
        "mcp-name",
    ]
    .into_iter()
    .any(|name| headers.get_all(name).iter().nth(1).is_some())
    {
        return error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            -32020,
            "HeaderMismatch: duplicate routing or authentication header",
        );
    }
    if !valid_origin(&headers) {
        return error(StatusCode::FORBIDDEN, Value::Null, -32600, "Invalid Origin");
    }
    if header_value(&headers, "authorization") != Some(&format!("Bearer {}", state.token)) {
        return error(
            StatusCode::UNAUTHORIZED,
            Value::Null,
            -32600,
            "Unauthorized",
        );
    }
    if !accepts_both(&headers) {
        return error(
            StatusCode::NOT_ACCEPTABLE,
            Value::Null,
            -32600,
            "Accept must include application/json and text/event-stream",
        );
    }
    if header_value(&headers, header::CONTENT_TYPE.as_str())
        .is_none_or(|value| !value.eq_ignore_ascii_case("application/json"))
    {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Value::Null,
            -32600,
            "Expected application/json",
        );
    }
    let body: Value = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(_) => return error(StatusCode::BAD_REQUEST, Value::Null, -32700, "Parse error"),
    };
    let id = body
        .get("id")
        .filter(|id| valid_request_id(id))
        .cloned()
        .unwrap_or(Value::Null);
    let Some(object) = body.as_object() else {
        return error(
            StatusCode::BAD_REQUEST,
            id,
            -32600,
            "Expected one JSON-RPC request",
        );
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || object.contains_key("result")
        || object.contains_key("error")
        || object.get("id").is_none_or(|id| !valid_request_id(id))
    {
        return error(
            StatusCode::BAD_REQUEST,
            id,
            -32600,
            "Expected one JSON-RPC request; batches, notifications and client responses are unsupported",
        );
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return error(StatusCode::BAD_REQUEST, id, -32600, "Missing method");
    };
    let params = match object.get("params") {
        None => None,
        Some(Value::Object(params)) => Some(params.clone()),
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                id,
                -32602,
                "Parameters must be an object",
            );
        }
    };
    let metadata_version = params
        .as_ref()
        .and_then(|params| params.get("_meta"))
        .and_then(Value::as_object)
        .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str);
    let version = header_value(&headers, "mcp-protocol-version");
    if version.is_none() || version != metadata_version {
        return error(
            StatusCode::BAD_REQUEST,
            id,
            -32020,
            "HeaderMismatch: MCP-Protocol-Version does not match params._meta",
        );
    }
    if version != Some(VERSION) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "jsonrpc":"2.0","id":id,
                "error":{"code":-32022,"message":"Unsupported protocol version",
                    "data":{"supported":[VERSION],"requested":version}}
            })),
        )
            .into_response();
    }
    if header_value(&headers, "mcp-method") != Some(method) {
        return error(
            StatusCode::BAD_REQUEST,
            id,
            -32020,
            "HeaderMismatch: Mcp-Method does not match method",
        );
    }
    if matches!(method, "tools/call" | "prompts/get" | "resources/read") {
        let Some(name) = params
            .as_ref()
            .and_then(|params| mirrored_name(method, params))
        else {
            return error(
                StatusCode::BAD_REQUEST,
                id,
                -32602,
                "Missing params.name or params.uri",
            );
        };
        if !matches_mirror(header_value(&headers, "mcp-name"), name) {
            return error(
                StatusCode::BAD_REQUEST,
                id,
                -32020,
                "HeaderMismatch: Mcp-Name does not match request",
            );
        }
    }
    // Tool schemas with x-mcp-header annotations are not tracked in this adapter.
    // Fail closed on supplied mirrored parameter headers; support for annotations
    // requires a request-scoped schema lookup and validation before forwarding.
    if headers
        .keys()
        .any(|key| key.as_str().starts_with("mcp-param-"))
    {
        return error(
            StatusCode::BAD_REQUEST,
            id,
            -32020,
            "HeaderMismatch: Mcp-Param headers are not supported by this adapter",
        );
    }
    if method == "initialize" || method.starts_with("notifications/") {
        return error(StatusCode::NOT_FOUND, id, -32601, "Method not found");
    }
    let (notification_tx, mut response_rx) = tokio_mpsc::channel(super::MAX_QUEUED_NOTIFICATIONS);
    let response_tx = super::StreamSender {
        tx: notification_tx,
        used: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let (terminal_tx, mut terminal_rx) = oneshot::channel();
    let message = BridgeMessage::Request {
        server_id: state.server_id.clone(),
        request_id: uuid::Uuid::new_v4().to_string(),
        http_id: id,
        method: method.into(),
        params,
        response_tx,
        terminal_tx,
    };
    let mut tx = state.tx.clone();
    if tx.send(message).await.is_err() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            Value::Null,
            -32603,
            "ACP bridge unavailable",
        );
    }
    let first = tokio::select! {
        biased;
        notification = response_rx.recv(), if !response_rx.is_closed() || !response_rx.is_empty() =>
            match notification {
                Some(mut message) => Some(std::mem::take(&mut message.value)),
                None => (&mut terminal_rx).await.ok(),
            },
        terminal = &mut terminal_rx => terminal.ok(),
    };
    let Some(first) = first else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            Value::Null,
            -32603,
            "ACP bridge closed",
        );
    };
    if first.get("id").is_some() {
        let status = if first.pointer("/error/code").and_then(Value::as_i64) == Some(-32601) {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::OK
        };
        return (status, Json(first)).into_response();
    }
    let stream = async_stream::stream! {
        yield Ok::<_, Infallible>(Event::default().data(first.to_string()));
        loop {
            // Drain already-queued notifications before a successful final response.
            // Overflow is delivered through the independent terminal path.
            let message = tokio::select! {
                biased;
                notification = response_rx.recv(), if !response_rx.is_closed() || !response_rx.is_empty() =>
                    match notification {
                        Some(mut message) => Some(std::mem::take(&mut message.value)),
                        None => (&mut terminal_rx).await.ok(),
                    },
                terminal = &mut terminal_rx => terminal.ok(),
            };
            let Some(message) = message else { break };
            let final_response = message.get("id").is_some();
            yield Ok::<_, Infallible>(Event::default().data(message.to_string()));
            if final_response { break }
        }
    };
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().expect("static header"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn accepts_only_both_media_types() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "accept",
            "application/json, text/event-stream".parse().unwrap(),
        );
        assert!(accepts_both(&headers));
        headers.insert("accept", "application/json".parse().unwrap());
        assert!(!accepts_both(&headers));
    }

    #[test]
    fn mirrored_names_decode_canonical_base64() {
        assert!(matches_mirror(
            Some("=?base64?SGVsbG8sIOS4lueVjA==?="),
            "Hello, 世界"
        ));
        assert!(matches_mirror(Some("simple"), "simple"));
        assert!(!matches_mirror(Some("=?base64?SGVsbG8=?="), "different"));
        assert!(!matches_mirror(Some("=?base64?SGVsbG8==?="), "Hello"));
        assert!(!matches_mirror(
            Some("=?base64?literal?="),
            "=?base64?literal?="
        ));
    }

    #[test]
    fn response_preserves_mrtr_and_opaque_request_state() {
        let result = serde_json::json!({
            "inputRequests": [{"method":"elicitation/create","params":{"message":"answer"}}],
            "requestState": {"opaque": [1, 2, 3]},
            "_meta": {"trace":"preserve", "io.modelcontextprotocol/subscriptionId":"unrelated"},
            "subscriptionId": "internal-id"
        });
        let response = rpc_result(serde_json::json!(42), "internal-id", result.clone());
        assert_eq!(response["result"], result);
        assert_eq!(response["id"], 42);
        let mapped = rpc_result(
            serde_json::json!("external"),
            "internal-id",
            serde_json::json!({"subscriptionId":"internal-id","requestState":"unchanged",
                "_meta":{"trace":"preserve", "io.modelcontextprotocol/subscriptionId":"internal-id",
                    "progressToken":"internal-id"}}),
        );
        assert_eq!(mapped["result"]["subscriptionId"], "internal-id");
        assert_eq!(
            mapped["result"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
            "external"
        );
        assert_eq!(mapped["result"]["_meta"]["progressToken"], "internal-id");
        assert_eq!(mapped["result"]["requestState"], "unchanged");
    }

    #[test]
    fn concurrent_logical_ids_with_same_external_id_stay_request_scoped() {
        let mut first =
            serde_json::json!({"_meta":{"io.modelcontextprotocol/subscriptionId":"one"}});
        let mut second =
            serde_json::json!({"_meta":{"io.modelcontextprotocol/subscriptionId":"two"}});
        rewrite_subscription_id(&mut first, "one", &serde_json::json!(7));
        rewrite_subscription_id(&mut second, "two", &serde_json::json!(7));
        assert_eq!(first["_meta"]["io.modelcontextprotocol/subscriptionId"], 7);
        assert_eq!(second["_meta"]["io.modelcontextprotocol/subscriptionId"], 7);
        let mut mismatch =
            serde_json::json!({"_meta":{"io.modelcontextprotocol/subscriptionId":"two"}});
        rewrite_subscription_id(&mut mismatch, "one", &serde_json::json!("7"));
        assert_eq!(
            mismatch["_meta"]["io.modelcontextprotocol/subscriptionId"],
            "two"
        );
    }

    #[tokio::test]
    async fn rejects_legacy_methods_and_invalid_headers_over_real_http() {
        async fn exchange(
            address: std::net::SocketAddr,
            method: &str,
            headers: &str,
            body: &str,
        ) -> String {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            let request = format!(
                "{method} / HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n{headers}Content-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let task = tokio::spawn(run_http_listener(
            listener,
            "server".into(),
            "secret".into(),
            tx,
        ));
        let legacy = exchange(address, "GET", "", "").await;
        assert!(legacy.starts_with("HTTP/1.1 405"), "{legacy}");
        let delete = exchange(address, "DELETE", "", "").await;
        assert!(delete.starts_with("HTTP/1.1 405"), "{delete}");
        let invalid_origin = exchange(address, "POST", "Origin: http://evil.test\r\n", "{}").await;
        assert!(
            invalid_origin.starts_with("HTTP/1.1 403"),
            "{invalid_origin}"
        );
        let invalid_auth = exchange(address, "POST", "", "{}").await;
        assert!(invalid_auth.starts_with("HTTP/1.1 401"), "{invalid_auth}");
        let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list",
            "params":{"_meta":{"io.modelcontextprotocol/protocolVersion":VERSION,
                "io.modelcontextprotocol/clientCapabilities":{}}}})
        .to_string();
        let headers = "Authorization: Bearer secret\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: wrong/method\r\n";
        let mismatch = exchange(address, "POST", headers, &body).await;
        assert!(mismatch.starts_with("HTTP/1.1 400"), "{mismatch}");
        assert!(mismatch.contains("-32020"), "{mismatch}");
        let batch = exchange(address, "POST", headers, "[]").await;
        assert!(batch.starts_with("HTTP/1.1 400"), "{batch}");
        let headers = headers.replace("wrong/method", "tools/list");
        let fractional_id = body.replace("\"id\":1", "\"id\":1.5");
        let fractional = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            exchange(address, "POST", &headers, &fractional_id),
        )
        .await
        .expect("an invalid request ID must be rejected before forwarding");
        assert!(fractional.starts_with("HTTP/1.1 400"), "{fractional}");
        assert!(fractional.contains("-32600"), "{fractional}");
        let error: Value =
            serde_json::from_str(fractional.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert!(error.get("id").is_none());
        task.abort();
    }
}
