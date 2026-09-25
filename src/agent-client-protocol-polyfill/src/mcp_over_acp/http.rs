//! MCP 2026-07-28 request-scoped Streamable HTTP endpoint.
//!
//! The adapter keeps raw MCP envelopes, ACP cancellation, and response-stream
//! lifetimes explicit. Each POST owns one operation, not an MCP session.

use std::{convert::Infallible, sync::Arc};

use agent_client_protocol::Error;
use axum::{
    Json, Router,
    body::{Body, HttpBody as _, to_bytes},
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::any,
};
use base64::Engine as _;
use futures::{SinkExt, StreamExt, channel::mpsc};
use hmac::{Hmac, Mac};
use serde_json::{Map, Value};
use sha2::Sha256;
use tokio::{
    net::TcpListener,
    sync::{Semaphore, mpsc as tokio_mpsc, oneshot},
};

use super::BridgeMessage;

const VERSION: &str = "2026-07-28";
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

fn server_route(server_id: &str) -> String {
    // Even an empty opaque ID must occupy a real route segment.
    format!(
        "mcp-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(server_id)
    )
}

pub(super) struct BridgeState {
    secret: [u8; 32],
    admission: Arc<Semaphore>,
    tx: mpsc::Sender<BridgeMessage>,
}

pub(super) async fn run_http_listener(
    listener: TcpListener,
    state: Arc<BridgeState>,
) -> Result<(), Error> {
    let app = Router::new()
        .route("/{route}", any(handle_request))
        .with_state(state);
    axum::serve(listener, app)
        .await
        .map_err(Error::into_internal_error)
}

impl BridgeState {
    pub(super) fn new(tx: mpsc::Sender<BridgeMessage>) -> Arc<Self> {
        Arc::new(Self {
            secret: {
                let mut secret = [0; 32];
                secret[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
                secret[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
                secret
            },
            admission: Arc::new(Semaphore::new(super::MAX_ACTIVE_REQUESTS)),
            tx,
        })
    }

    fn mac(&self, server_id: &str) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret).expect("SHA-256 HMAC key");
        mac.update(b"mcp-over-acp-http-adapter/server/v1\0");
        mac.update(server_id.as_bytes());
        mac
    }

    pub(super) fn declaration_url(&self, port: u16, server_id: &str) -> (String, String) {
        let route = server_route(server_id);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(self.mac(server_id).finalize().into_bytes());
        (format!("http://127.0.0.1:{port}/{route}"), token)
    }
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

pub(super) fn rpc_binding_error(id: Value, error: Error) -> Value {
    let value = serde_json::to_value(error).unwrap_or(Value::Null);
    let peer_code = value.get("code").and_then(Value::as_i64);
    let code = match peer_code {
        Some(-33000 | -33001 | -33002 | -32800) => peer_code.unwrap(),
        _ => -33002,
    };
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("MCP binding failure");
    rpc_error(id, code, message)
}

pub(super) fn rpc_peer_error(id: Value, error: Value) -> Value {
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
    let mut json = false;
    let mut sse = false;
    for value in headers.get_all(header::ACCEPT) {
        let Ok(value) = value.to_str() else {
            return false;
        };
        for item in value.split(',') {
            let mut parts = item.split(';');
            let media = parts.next().unwrap_or("").trim();
            let mut quality = 1.0;
            for part in parts {
                if let Some((key, q)) = part.trim().split_once('=')
                    && key.trim().eq_ignore_ascii_case("q")
                {
                    quality = q.trim().parse::<f32>().unwrap_or(0.0);
                }
            }
            if quality <= 0.0 || quality > 1.0 {
                continue;
            }
            json |= media.eq_ignore_ascii_case("application/json");
            sse |= media.eq_ignore_ascii_case("text/event-stream");
        }
    }
    json && sse
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
        !(header.starts_with("=?base64?") && header.ends_with("?=")) && header == body
    }
}

async fn handle_request(
    State(state): State<Arc<BridgeState>>,
    Path(route): Path<String>,
    method: axum::http::Method,
    headers: HeaderMap,
    body: Body,
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
    if route.len() > 4096 {
        return error(
            StatusCode::NOT_FOUND,
            Value::Null,
            -32601,
            "Unknown MCP route",
        );
    }
    let server_id = {
        let decoded = route.strip_prefix("mcp-").and_then(|encoded| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .ok()
        });
        let Some(server_id) = decoded
            .and_then(|id| String::from_utf8(id).ok())
            .filter(|id| server_route(id) == route)
        else {
            return error(
                StatusCode::NOT_FOUND,
                Value::Null,
                -32601,
                "Unknown MCP route",
            );
        };
        let authorization =
            header_value(&headers, "authorization").and_then(|value| value.split_once(' '));
        if !authorization.is_some_and(|(scheme, supplied)| {
            scheme.eq_ignore_ascii_case("bearer")
                && base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(supplied)
                    .is_ok_and(|tag| state.mac(&server_id).verify_slice(&tag).is_ok())
        }) {
            let mut response = error(
                StatusCode::UNAUTHORIZED,
                Value::Null,
                -32600,
                "Unauthorized",
            );
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                "Bearer".parse().expect("static header"),
            );
            return response;
        }
        server_id
    };
    if method != axum::http::Method::POST {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            Value::Null,
            -32600,
            "Only POST is supported",
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
    if header_value(&headers, header::CONTENT_TYPE.as_str()).is_none_or(|value| {
        !value
            .split(';')
            .next()
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
    }) {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Value::Null,
            -32600,
            "Expected application/json",
        );
    }
    // Acquire before reading a potentially slow/large request body. The permit
    // stays owned by the response body until the client consumes or drops it.
    let Ok(permit) = state.admission.clone().try_acquire_owned() else {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            Value::Null,
            -33000,
            "Too many outstanding MCP responses",
        );
    };
    let response = handle_admitted_request(state, server_id, headers, body).await;
    let (mut parts, body) = response.into_parts();
    if let Some(length) = body.size_hint().exact() {
        parts
            .headers
            .entry(header::CONTENT_LENGTH)
            .or_insert_with(|| length.to_string().parse().expect("decimal body length"));
    }
    // One ownership rule for every admitted response, including validation
    // failures that echo a potentially large, but valid, external request ID.
    let stream = async_stream::stream! {
        let _permit = permit;
        let mut body = body.into_data_stream();
        while let Some(chunk) = body.next().await {
            yield chunk;
        }
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

async fn handle_admitted_request(
    state: Arc<BridgeState>,
    server_id: String,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Ok(body) = to_bytes(body, MAX_REQUEST_BODY_BYTES).await else {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            Value::Null,
            -33000,
            "Request body too large",
        );
    };
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
    // This endpoint re-exports native tools without transport-only x-mcp-header
    // annotations. Mirrored parameter headers have no authority here.
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
    let id_for_bridge_error = id.clone();
    let (notification_tx, mut response_rx) = tokio_mpsc::channel(super::MAX_QUEUED_NOTIFICATIONS);
    let response_tx = super::StreamSender {
        tx: notification_tx,
        used: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let (terminal_tx, mut terminal_rx) = oneshot::channel();
    let message = BridgeMessage::Request {
        server_id,
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
            id_for_bridge_error.clone(),
            -33002,
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
            id_for_bridge_error,
            -33002,
            "ACP bridge closed",
        );
    };
    if first.get("id").is_some() {
        let status = if first.pointer("/error/code").and_then(Value::as_i64) == Some(-32601) {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::OK
        };
        let payload = first.to_string();
        let length = payload.len().to_string();
        let stream = async_stream::stream! {
            yield Ok::<_, Infallible>(axum::body::Bytes::from(payload));
        };
        return (
            status,
            [
                (header::CONTENT_TYPE, "application/json".to_string()),
                (header::CONTENT_LENGTH, length),
            ],
            Body::from_stream(stream),
        )
            .into_response();
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
    fn stateless_declarations_do_not_allocate_routes() {
        let (tx, _rx) = mpsc::channel(1);
        let state = BridgeState::new(tx);
        let first = state.declaration_url(1234, "server/one");
        let other = state.declaration_url(1234, "server/two");
        assert_eq!(first, state.declaration_url(1234, "server/one"));
        assert_ne!(first, other);
        assert!(state.declaration_url(1234, "").0.ends_with("/mcp-"));
        for i in 0..1000 {
            let (url, bearer) = state.declaration_url(1234, &i.to_string());
            assert!(url.starts_with("http://127.0.0.1:1234/"));
            assert!(!url.contains(&bearer));
        }
    }

    #[tokio::test]
    async fn bridge_failure_preserves_valid_external_id() {
        let (tx, rx) = mpsc::channel(1);
        let state = BridgeState::new(tx);
        drop(rx);
        let (_, token) = state.declaration_url(8000, "server");
        let mut headers = HeaderMap::new();
        headers.insert("host", "127.0.0.1:8000".parse().unwrap());
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers.insert(
            "accept",
            "application/json, text/event-stream".parse().unwrap(),
        );
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("mcp-protocol-version", VERSION.parse().unwrap());
        headers.insert("mcp-method", "tools/list".parse().unwrap());
        let response = handle_request(
            State(state),
            Path(server_route("server")),
            axum::http::Method::POST,
            headers,
            Body::from(
                serde_json::json!({"jsonrpc":"2.0","id":"external",
                "method":"tools/list","params":{"_meta":{
                    "io.modelcontextprotocol/protocolVersion":VERSION,
                    "io.modelcontextprotocol/clientCapabilities":{}}}})
                .to_string(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["id"], "external");
        assert_eq!(body["error"]["code"], -33002);
    }

    #[tokio::test]
    async fn unread_validation_errors_hold_admission_until_consumed_or_dropped() {
        let (tx, _rx) = mpsc::channel(1);
        let state = BridgeState::new(tx);
        let (_, token) = state.declaration_url(8000, "server");
        let route = server_route("server");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers.insert(
            "accept",
            "application/json, text/event-stream".parse().unwrap(),
        );
        headers.insert("content-type", "application/json".parse().unwrap());
        // No method: validation must echo this large known ID without releasing
        // the permit while the client still owns its unread response.
        let id = "external".repeat(32 * 1024);
        let body = serde_json::json!({"jsonrpc":"2.0", "id":id}).to_string();
        let send = || {
            handle_request(
                State(state.clone()),
                Path(route.clone()),
                axum::http::Method::POST,
                headers.clone(),
                Body::from(body.clone()),
            )
        };
        let mut responses = Vec::new();
        for _ in 0..super::super::MAX_ACTIVE_REQUESTS {
            let response = send().await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            responses.push(response);
        }
        assert_eq!(send().await.status(), StatusCode::TOO_MANY_REQUESTS);

        let bytes = to_bytes(responses.pop().unwrap().into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let error: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error["id"], id);
        assert_eq!(error["error"]["code"], -32600);
        assert_eq!(state.admission.available_permits(), 1);
        responses.push(send().await);
        assert_eq!(state.admission.available_permits(), 0);

        drop(responses.pop());
        assert_eq!(state.admission.available_permits(), 1);
        let recovered = send().await;
        assert_eq!(recovered.status(), StatusCode::BAD_REQUEST);
        drop(recovered);
        drop(responses);
        assert_eq!(
            state.admission.available_permits(),
            super::super::MAX_ACTIVE_REQUESTS
        );
    }

    #[tokio::test]
    async fn unread_terminal_bodies_hold_admission_until_drop() {
        let (tx, mut rx) = mpsc::channel(128);
        let state = BridgeState::new(tx);
        let (_, token) = state.declaration_url(8000, "server");
        let route = server_route("server");
        let mut headers = HeaderMap::new();
        headers.insert("host", "127.0.0.1:8000".parse().unwrap());
        headers.insert("authorization", format!("bearer {token}").parse().unwrap());
        headers.insert("accept", "application/json".parse().unwrap());
        headers.append("accept", "text/event-stream;q=0.8".parse().unwrap());
        headers.insert(
            "content-type",
            "application/json; charset=utf-8".parse().unwrap(),
        );
        headers.insert("mcp-protocol-version", VERSION.parse().unwrap());
        headers.insert("mcp-method", "tools/list".parse().unwrap());
        tokio::spawn(async move {
            while let Some(BridgeMessage::Request {
                terminal_tx,
                http_id,
                ..
            }) = rx.next().await
            {
                drop(terminal_tx.send(rpc_result(http_id, "", serde_json::json!({"tools":[]}))));
            }
        });
        let body = serde_json::json!({"jsonrpc":"2.0","id":"known","method":"tools/list",
            "params":{"_meta":{"io.modelcontextprotocol/protocolVersion":VERSION,
                "io.modelcontextprotocol/clientCapabilities":{}}}})
        .to_string();
        let send = || {
            handle_request(
                State(state.clone()),
                Path(route.clone()),
                axum::http::Method::POST,
                headers.clone(),
                Body::from(body.clone()),
            )
        };
        let mut responses = Vec::new();
        for _ in 0..super::super::MAX_ACTIVE_REQUESTS {
            let response = send().await;
            assert_eq!(response.status(), StatusCode::OK);
            responses.push(response);
        }
        assert_eq!(send().await.status(), StatusCode::TOO_MANY_REQUESTS);
        drop(responses.pop());
        let recovered = send().await;
        assert_eq!(recovered.status(), StatusCode::OK);
    }

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
        headers.append("accept", "text/event-stream;q=0.9".parse().unwrap());
        assert!(accepts_both(&headers));
        headers.insert(
            "accept",
            "application/json, text/event-stream;q=0".parse().unwrap(),
        );
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
        assert!(matches_mirror(
            Some("=?base64?unfinished"),
            "=?base64?unfinished"
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
            route: &str,
            method: &str,
            headers: &str,
            body: &str,
        ) -> String {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            let request = format!(
                "{method} /{route} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n{headers}Content-Length: {}\r\n\r\n{body}",
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
        let state = BridgeState::new(tx);
        let (url, token) = state.declaration_url(address.port(), "server");
        let route = url.rsplit('/').next().unwrap();
        let task = tokio::spawn(run_http_listener(listener, state));
        let auth = format!("Authorization: Bearer {token}\r\n");
        let legacy = exchange(address, route, "GET", &auth, "").await;
        assert!(legacy.starts_with("HTTP/1.1 405"), "{legacy}");
        let delete = exchange(address, route, "DELETE", &auth, "").await;
        assert!(delete.starts_with("HTTP/1.1 405"), "{delete}");
        let invalid_origin =
            exchange(address, route, "POST", "Origin: http://evil.test\r\n", "{}").await;
        assert!(
            invalid_origin.starts_with("HTTP/1.1 403"),
            "{invalid_origin}"
        );
        let invalid_get_origin =
            exchange(address, route, "GET", "Origin: http://evil.test\r\n", "").await;
        assert!(
            invalid_get_origin.starts_with("HTTP/1.1 403"),
            "{invalid_get_origin}"
        );
        let invalid_auth = exchange(address, route, "POST", "", "{}").await;
        assert!(invalid_auth.starts_with("HTTP/1.1 401"), "{invalid_auth}");
        assert!(
            invalid_auth
                .to_ascii_lowercase()
                .contains("www-authenticate: bearer"),
            "{invalid_auth}"
        );
        let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list",
            "params":{"_meta":{"io.modelcontextprotocol/protocolVersion":VERSION,
                "io.modelcontextprotocol/clientCapabilities":{}}}})
        .to_string();
        let headers = format!(
            "{auth}Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: wrong/method\r\n"
        );
        let mismatch = exchange(address, route, "POST", &headers, &body).await;
        assert!(mismatch.starts_with("HTTP/1.1 400"), "{mismatch}");
        assert!(mismatch.contains("-32020"), "{mismatch}");
        let batch = exchange(address, route, "POST", &headers, "[]").await;
        assert!(batch.starts_with("HTTP/1.1 400"), "{batch}");
        let headers = headers.replace("wrong/method", "tools/list");
        let fractional_id = body.replace("\"id\":1", "\"id\":1.5");
        let fractional = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            exchange(address, route, "POST", &headers, &fractional_id),
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
