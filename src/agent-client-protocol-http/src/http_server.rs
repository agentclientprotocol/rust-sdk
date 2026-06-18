use std::{convert::Infallible, error::Error as _, sync::Arc, time::Duration};

use agent_client_protocol::{RawJsonRpcMessage, schema::v1::Response as RpcResponse};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Response, Sse, sse::Event},
};
use tracing::{error, info, trace};

use crate::{
    connection::{Connection, ConnectionRegistry, ResponseRoute},
    protocol::{
        EVENT_STREAM_MIME_TYPE, HEADER_CONNECTION_ID, HEADER_SESSION_ID, JSON_MIME_TYPE,
        apply_session_header_to_message, is_initialize_request, method_for_message,
        method_requires_session_header, session_id_from_message,
    },
};

const MAX_POST_BODY_BYTES: usize = 16 * 1024 * 1024;

pub(crate) async fn handle_post(
    State(registry): State<Arc<ConnectionRegistry>>,
    request: Request<Body>,
) -> Response {
    if !request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with(JSON_MIME_TYPE))
    {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json",
        )
            .into_response();
    }

    let connection_id = header_value(request.headers(), HEADER_CONNECTION_ID);
    let session_id = header_value(request.headers(), HEADER_SESSION_ID);
    if content_length_exceeds_limit(request.headers()) {
        return post_body_too_large_response();
    }

    let body = match axum::body::to_bytes(request.into_body(), MAX_POST_BODY_BYTES).await {
        Ok(body) => body,
        Err(e) => {
            error!("Failed to read request body: {e}");
            if is_body_limit_error(&e) {
                return post_body_too_large_response();
            }
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    if matches!(body.first(), Some(&b'[')) {
        return StatusCode::NOT_IMPLEMENTED.into_response();
    }

    let mut message = match serde_json::from_slice::<RawJsonRpcMessage>(&body) {
        Ok(message) => message,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON-RPC: {e}")).into_response();
        }
    };

    if is_initialize_request(&message) {
        let (connection_id, connection) = registry.create_connection().await;
        let initialize_cleanup =
            InitializeCleanup::new(registry.clone(), connection_id.clone(), connection.clone());
        if connection.send_to_agent(message).is_err() {
            initialize_cleanup.cleanup().await;
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }

        let Some(init_response) = connection.recv_initial().await else {
            initialize_cleanup.cleanup().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "agent closed before initialize response",
            )
                .into_response();
        };
        let initialize_failed = matches!(
            init_response,
            RawJsonRpcMessage::Response(RpcResponse::Error { .. })
        );
        let init_response = match serde_json::to_string(&init_response) {
            Ok(response) => response,
            Err(e) => {
                initialize_cleanup.cleanup().await;
                error!("failed to serialize initialize response: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        if initialize_failed {
            initialize_cleanup.cleanup().await;
            info!(connection_id = %connection_id, "Initialize rejected");
            return json_response(init_response);
        }

        connection.start_router().await;
        initialize_cleanup.disarm();
        info!(connection_id = %connection_id, "Initialize complete");
        return with_connection_header(json_response(init_response), &connection_id);
    }

    let Some(connection_id) = connection_id else {
        return (StatusCode::BAD_REQUEST, "Acp-Connection-Id header required").into_response();
    };
    let Some(connection) = registry.get(&connection_id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if let Some(session_id) = &session_id
        && method_for_message(&message).is_some()
        && let Err(error) = apply_session_header_to_message(&mut message, session_id)
    {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }

    let route = match method_for_message(&message) {
        Some(method) => match session_id_from_message(&message) {
            Some(session_id) => Some(ResponseRoute::Session(session_id)),
            None if method_requires_session_header(method) => {
                return (StatusCode::BAD_REQUEST, "Acp-Session-Id header required").into_response();
            }
            None => Some(ResponseRoute::Connection),
        },
        None => None,
    };

    if let Some(ResponseRoute::Session(session_id)) = &route {
        connection.ensure_session(session_id).await;
    }
    if let (RawJsonRpcMessage::Request(req), Some(route)) = (&message, route) {
        connection.record_pending_route(req.id.clone(), route).await;
        trace!(connection_id = %connection_id, method = %req.method, "POST → agent");
    } else {
        trace!(connection_id = %connection_id, ?message, "POST → agent");
    }

    if connection.send_to_agent(message).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    StatusCode::ACCEPTED.into_response()
}

struct InitializeCleanup {
    registry: Option<Arc<ConnectionRegistry>>,
    connection_id: String,
    connection: Arc<Connection>,
}

impl InitializeCleanup {
    fn new(
        registry: Arc<ConnectionRegistry>,
        connection_id: String,
        connection: Arc<Connection>,
    ) -> Self {
        Self {
            registry: Some(registry),
            connection_id,
            connection,
        }
    }

    async fn cleanup(mut self) {
        self.cleanup_inner().await;
    }

    fn disarm(mut self) {
        self.registry.take();
    }

    async fn cleanup_inner(&mut self) {
        let Some(registry) = self.registry.take() else {
            return;
        };
        registry.remove(&self.connection_id).await;
        self.connection.shutdown().await;
    }
}

impl Drop for InitializeCleanup {
    fn drop(&mut self) {
        let Some(registry) = self.registry.take() else {
            return;
        };
        let connection_id = self.connection_id.clone();
        let connection = self.connection.clone();
        tokio::spawn(async move {
            registry.remove(&connection_id).await;
            connection.shutdown().await;
        });
    }
}

pub(crate) async fn handle_get(
    registry: Arc<ConnectionRegistry>,
    request: Request<Body>,
) -> Response {
    if !request
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains(EVENT_STREAM_MIME_TYPE))
    {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "client must accept text/event-stream",
        )
            .into_response();
    }

    let Some(connection_id) = header_value(request.headers(), HEADER_CONNECTION_ID) else {
        return (StatusCode::BAD_REQUEST, "Acp-Connection-Id header required").into_response();
    };
    let Some(connection) = registry.get(&connection_id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let session_id = header_value(request.headers(), HEADER_SESSION_ID);
    let (replay, mut receiver) = match session_id.as_deref() {
        Some(session_id) => connection.subscribe_session_stream(session_id).await,
        None => connection.subscribe_connection_stream().await,
    };
    let mut closed = connection.subscribe_closed();
    let stream = async_stream::stream! {
        for msg in replay {
            trace!(payload = %msg, "SSE → client (replay)");
            yield Ok::<_, Infallible>(Event::default().data(msg));
        }
        loop {
            if *closed.borrow() {
                break;
            }
            tokio::select! {
                recv = receiver.recv() => match recv {
                    Some(msg) => {
                        trace!(payload = %msg, "SSE → client");
                        yield Ok(Event::default().data(msg));
                    }
                    None => break,
                },
                changed = closed.changed() => {
                    if changed.is_err() || *closed.borrow() {
                        break;
                    }
                }
            }
        }
    };

    let mut response = with_connection_header(
        Sse::new(stream)
            .keep_alive(
                axum::response::sse::KeepAlive::new()
                    .interval(Duration::from_secs(15))
                    .text(""),
            )
            .into_response(),
        &connection_id,
    );
    if let Some(session_id) = session_id
        && let Ok(value) = HeaderValue::from_str(&session_id)
    {
        response.headers_mut().insert(HEADER_SESSION_ID, value);
    }
    response
}

pub(crate) async fn handle_delete(
    State(registry): State<Arc<ConnectionRegistry>>,
    request: Request<Body>,
) -> Response {
    let Some(connection_id) = header_value(request.headers(), HEADER_CONNECTION_ID) else {
        return (StatusCode::BAD_REQUEST, "Acp-Connection-Id header required").into_response();
    };
    let Some(connection) = registry.remove(&connection_id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    connection.shutdown().await;
    info!(connection_id = %connection_id, "Connection terminated via DELETE");
    StatusCode::ACCEPTED.into_response()
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

fn with_connection_header(mut response: Response, connection_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(connection_id) {
        response.headers_mut().insert(HEADER_CONNECTION_ID, value);
    }
    response
}

fn json_response(body: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, JSON_MIME_TYPE)],
        body,
    )
        .into_response()
}

fn content_length_exceeds_limit(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_POST_BODY_BYTES)
}

fn is_body_limit_error(error: &axum::Error) -> bool {
    let mut source = error.source();
    while let Some(error) = source {
        if error.to_string() == "length limit exceeded" {
            return true;
        }
        source = error.source();
    }
    false
}

fn post_body_too_large_response() -> Response {
    (StatusCode::PAYLOAD_TOO_LARGE, "POST body too large").into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_client_protocol::{Channel, RawJsonRpcMessage, schema::v1::RequestId};
    use futures::{StreamExt, future::BoxFuture};
    use serde_json::json;
    use tokio::{
        sync::mpsc,
        time::{Duration, sleep, timeout},
    };

    use super::*;
    use crate::connection::{AgentFactory, OUTBOUND_STREAM_CAPACITY};

    struct CapturingAgentFactory {
        forwarded: mpsc::UnboundedSender<RawJsonRpcMessage>,
    }

    impl AgentFactory for CapturingAgentFactory {
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (agent, transport) = Channel::duplex();
            let forwarded = self.forwarded.clone();
            let future = Box::pin(async move {
                let Channel {
                    rx: mut incoming,
                    tx: _,
                } = agent;
                while let Some(message) = incoming.next().await {
                    if forwarded.send(message?).is_err() {
                        break;
                    }
                }
                Ok(())
            });

            (transport, future)
        }
    }

    struct RejectingInitializeAgentFactory;

    impl AgentFactory for RejectingInitializeAgentFactory {
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (mut agent, transport) = Channel::duplex();
            let future = Box::pin(async move {
                if let Some(Ok(RawJsonRpcMessage::Request(request))) = agent.rx.next().await {
                    agent
                        .tx
                        .unbounded_send(Ok(RawJsonRpcMessage::response(
                            request.id,
                            Err(agent_client_protocol::Error::invalid_request()
                                .data("initialize rejected")),
                        )))
                        .unwrap();
                }
                std::future::pending::<agent_client_protocol::Result<()>>().await
            });

            (transport, future)
        }
    }

    struct PendingInitializeAgentFactory;

    impl AgentFactory for PendingInitializeAgentFactory {
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (agent, transport) = Channel::duplex();
            let future = Box::pin(async move {
                let Channel {
                    rx: mut incoming,
                    tx: _outgoing,
                } = agent;
                drop(incoming.next().await);
                std::future::pending::<agent_client_protocol::Result<()>>().await
            });

            (transport, future)
        }
    }

    #[tokio::test]
    async fn post_rejects_declared_body_larger_than_limit() {
        let (forwarded_tx, _forwarded_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(CapturingAgentFactory {
            forwarded: forwarded_tx,
        })));
        let request = Request::builder()
            .method("POST")
            .uri("/acp")
            .header(header::CONTENT_TYPE, JSON_MIME_TYPE)
            .header(
                header::CONTENT_LENGTH,
                (MAX_POST_BODY_BYTES + 1).to_string(),
            )
            .body(Body::from("{}"))
            .unwrap();

        let response = handle_post(State(registry), request).await;

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn initialize_error_response_rejects_connection() {
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(
            RejectingInitializeAgentFactory,
        )));
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        })
        .to_string();
        let request = Request::builder()
            .method("POST")
            .uri("/acp")
            .header(header::CONTENT_TYPE, JSON_MIME_TYPE)
            .body(Body::from(body))
            .unwrap();

        let response = handle_post(State(registry.clone()), request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(HEADER_CONNECTION_ID).is_none());
        assert_eq!(registry.len().await, 0);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let message = serde_json::from_slice::<RawJsonRpcMessage>(&body).unwrap();
        assert!(matches!(
            message,
            RawJsonRpcMessage::Response(RpcResponse::Error {
                id: RequestId::Number(1),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn cancelled_initialize_cleans_up_connection() {
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(
            PendingInitializeAgentFactory,
        )));
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        })
        .to_string();
        let request = Request::builder()
            .method("POST")
            .uri("/acp")
            .header(header::CONTENT_TYPE, JSON_MIME_TYPE)
            .body(Body::from(body))
            .unwrap();

        {
            let initialize = handle_post(State(registry.clone()), request);
            tokio::pin!(initialize);
            timeout(Duration::from_secs(1), async {
                loop {
                    tokio::select! {
                        response = &mut initialize => {
                            panic!(
                                "initialize completed unexpectedly with {}",
                                response.status()
                            );
                        }
                        () = sleep(Duration::from_millis(10)) => {
                            if registry.len().await == 1 {
                                break;
                            }
                        }
                    }
                }
            })
            .await
            .unwrap();
        }

        timeout(Duration::from_secs(1), async {
            loop {
                if registry.len().await == 0 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sse_closes_slow_subscriber_before_skipping_messages() {
        let (forwarded_tx, _forwarded_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(CapturingAgentFactory {
            forwarded: forwarded_tx,
        })));
        let (connection_id, connection) = registry.create_connection().await;
        let request = Request::builder()
            .method("GET")
            .uri("/acp")
            .header(header::ACCEPT, EVENT_STREAM_MIME_TYPE)
            .header(HEADER_CONNECTION_ID, connection_id.as_str())
            .body(Body::empty())
            .unwrap();
        let response = handle_get(registry, request).await;
        assert_eq!(response.status(), StatusCode::OK);

        for i in 0..=OUTBOUND_STREAM_CAPACITY {
            connection
                .push_connection_stream_for_test(format!("message-{i}"))
                .await;
        }

        let body = timeout(
            Duration::from_secs(1),
            axum::body::to_bytes(response.into_body(), 1024 * 1024),
        )
        .await
        .unwrap()
        .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("message-0"));
        assert!(body.contains(&format!("message-{}", OUTBOUND_STREAM_CAPACITY - 1)));
        assert!(!body.contains(&format!("message-{OUTBOUND_STREAM_CAPACITY}")));

        connection.shutdown().await;
    }

    #[tokio::test]
    async fn post_forwards_header_session_id_to_agent_params() {
        let (forwarded_tx, mut forwarded_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(CapturingAgentFactory {
            forwarded: forwarded_tx,
        })));
        let (connection_id, connection) = registry.create_connection().await;
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/prompt",
            "params": { "prompt": [] }
        })
        .to_string();
        let request = Request::builder()
            .method("POST")
            .uri("/acp")
            .header(header::CONTENT_TYPE, JSON_MIME_TYPE)
            .header(HEADER_CONNECTION_ID, connection_id.as_str())
            .header(HEADER_SESSION_ID, "session-1")
            .body(Body::from(body))
            .unwrap();

        let response = handle_post(State(registry), request).await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            session_id_from_message(&forwarded).as_deref(),
            Some("session-1")
        );
        connection.shutdown().await;
    }

    #[tokio::test]
    async fn post_does_not_apply_session_header_to_cancel_request() {
        let (forwarded_tx, mut forwarded_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(CapturingAgentFactory {
            forwarded: forwarded_tx,
        })));
        let (connection_id, connection) = registry.create_connection().await;
        let body = json!({
            "jsonrpc": "2.0",
            "method": "$/cancel_request",
            "params": { "requestId": 1 }
        })
        .to_string();
        let request = Request::builder()
            .method("POST")
            .uri("/acp")
            .header(header::CONTENT_TYPE, JSON_MIME_TYPE)
            .header(HEADER_CONNECTION_ID, connection_id.as_str())
            .header(HEADER_SESSION_ID, "session-1")
            .body(Body::from(body))
            .unwrap();

        let response = handle_post(State(registry), request).await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session_id_from_message(&forwarded), None);
        let value = serde_json::to_value(forwarded).unwrap();
        assert_eq!(value["params"], json!({ "requestId": 1 }));
        connection.shutdown().await;
    }

    #[tokio::test]
    async fn post_rejects_session_scoped_method_without_session_id() {
        let (forwarded_tx, mut forwarded_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(CapturingAgentFactory {
            forwarded: forwarded_tx,
        })));
        let (connection_id, connection) = registry.create_connection().await;

        for method in ["session/delete", "session/fork"] {
            let body = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": {}
            })
            .to_string();
            let request = Request::builder()
                .method("POST")
                .uri("/acp")
                .header(header::CONTENT_TYPE, JSON_MIME_TYPE)
                .header(HEADER_CONNECTION_ID, connection_id.as_str())
                .body(Body::from(body))
                .unwrap();

            let response = handle_post(State(registry.clone()), request).await;

            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{method}");
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert_eq!(body.as_ref(), b"Acp-Session-Id header required", "{method}");
            assert!(forwarded_rx.try_recv().is_err(), "{method}");
        }
        connection.shutdown().await;
    }
}
