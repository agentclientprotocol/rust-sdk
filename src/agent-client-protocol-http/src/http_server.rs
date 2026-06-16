use std::{convert::Infallible, sync::Arc, time::Duration};

use agent_client_protocol::RawJsonRpcMessage;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Response, Sse, sse::Event},
};
use tokio::sync::broadcast;
use tracing::{debug, error, info, trace};

use crate::{
    connection::{ConnectionRegistry, ResponseRoute},
    protocol::{
        EVENT_STREAM_MIME_TYPE, HEADER_CONNECTION_ID, HEADER_SESSION_ID, JSON_MIME_TYPE,
        apply_session_header_to_message, is_initialize_request, method_for_message,
        method_requires_session_header, session_id_from_message,
    },
};

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
    let body = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(e) => {
            error!("Failed to read request body: {e}");
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
        if connection.send_to_agent(message).is_err() {
            registry.remove(&connection_id).await;
            connection.shutdown().await;
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }

        let Some(init_response) = connection.recv_initial().await else {
            registry.remove(&connection_id).await;
            connection.shutdown().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "agent closed before initialize response",
            )
                .into_response();
        };

        connection.start_router().await;
        info!(connection_id = %connection_id, "Initialize complete");
        return with_connection_header(
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, JSON_MIME_TYPE)],
                init_response,
            )
                .into_response(),
            &connection_id,
        );
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
    let stream = async_stream::stream! {
        for msg in replay {
            trace!(payload = %msg, "SSE → client (replay)");
            yield Ok::<_, Infallible>(Event::default().data(msg));
        }
        loop {
            match receiver.recv().await {
                Ok(msg) => {
                    trace!(payload = %msg, "SSE → client");
                    yield Ok(Event::default().data(msg));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => debug!("SSE subscriber lagged {n} messages"),
                Err(broadcast::error::RecvError::Closed) => break,
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_client_protocol::{Channel, RawJsonRpcMessage};
    use futures::{StreamExt, future::BoxFuture};
    use serde_json::json;
    use tokio::{
        sync::mpsc,
        time::{Duration, timeout},
    };

    use super::*;
    use crate::connection::AgentFactory;

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
}
