use std::{collections::HashSet, sync::Arc};

use agent_client_protocol::{
    Agent, Channel, Client, ConnectTo, Error as AcpError, RawJsonRpcMessage,
    schema::Response as RpcResponse,
};
use futures::{SinkExt, StreamExt, future::BoxFuture};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, trace, warn};

use crate::protocol::{
    HEADER_CONNECTION_ID, HEADER_SESSION_ID, is_initialize_request, method_for_message,
    method_requires_session_header, session_id_from_message,
};

#[derive(Debug, Error)]
pub enum HttpClientError {
    #[error("invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("failed to build HTTP client: {0}")]
    Reqwest(#[from] reqwest::Error),
}

pub struct HttpClient {
    endpoint: url::Url,
    http: reqwest::Client,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("endpoint", &self.endpoint.as_str())
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    pub fn new(base_url: impl AsRef<str>) -> Result<Self, HttpClientError> {
        Self::with_client(base_url, reqwest::Client::new())
    }

    pub fn with_client(
        base_url: impl AsRef<str>,
        http: reqwest::Client,
    ) -> Result<Self, HttpClientError> {
        let mut endpoint = url::Url::parse(base_url.as_ref())?;
        let path = endpoint.path().trim_end_matches('/').to_string();
        let path = if path.is_empty() {
            "/acp".to_string()
        } else if path.ends_with("/acp") {
            path
        } else {
            format!("{path}/acp")
        };
        endpoint.set_path(&path);
        Ok(Self { endpoint, http })
    }

    fn is_websocket(&self) -> bool {
        matches!(self.endpoint.scheme(), "ws" | "wss")
    }
}

impl ConnectTo<Client> for HttpClient {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<(), AcpError> {
        let (channel, transport) = ConnectTo::<Client>::into_channel_and_future(self);
        match futures::future::select(
            std::pin::pin!(client.connect_to(channel)),
            std::pin::pin!(transport),
        )
        .await
        {
            futures::future::Either::Left((result, _))
            | futures::future::Either::Right((result, _)) => result,
        }
    }

    fn into_channel_and_future(self) -> (Channel, BoxFuture<'static, Result<(), AcpError>>) {
        let (caller, transport) = Channel::duplex();
        (caller, Box::pin(run(self, transport)))
    }
}

async fn run(client: HttpClient, channel: Channel) -> Result<(), AcpError> {
    if client.is_websocket() {
        return run_ws(client, channel).await;
    }
    let HttpClient { endpoint, http } = client;
    let Channel {
        rx: mut outgoing,
        tx: incoming,
    } = channel;
    let (open_session_tx, mut open_session_rx) = futures::channel::mpsc::unbounded();
    let state = Arc::new(ClientState {
        endpoint,
        http,
        connection_id: Mutex::new(None),
        open_session_streams: Mutex::new(HashSet::new()),
        incoming,
        open_session_tx,
    });
    let mut sse_tasks = Vec::new();

    let result = loop {
        let msg = tokio::select! {
            msg = outgoing.next() => match msg {
                Some(Ok(msg)) => msg,
                Some(Err(e)) => {
                    error!("upstream channel produced error: {e}");
                    break Err(e);
                }
                None => break Ok(()),
            },
            Some(session_id) = open_session_rx.next() => {
                spawn_sse(state.clone(), Some(session_id), &mut sse_tasks);
                continue;
            }
        };

        if state.connection_id.lock().await.is_none() {
            if !is_initialize_request(&msg) {
                break Err(AcpError::invalid_request()
                    .data("ACP HTTP transport: first message must be `initialize`"));
            }
            if let Err(e) = state.initialize(msg).await {
                error!("initialize failed: {e}");
                break Err(AcpError::internal_error().data(format!("initialize: {e}")));
            }
            spawn_sse(state.clone(), None, &mut sse_tasks);
            continue;
        }

        if let Some(session_id) = session_id_from_message(&msg)
            && state
                .open_session_streams
                .lock()
                .await
                .insert(session_id.clone())
        {
            spawn_sse(state.clone(), Some(session_id), &mut sse_tasks);
        }

        if let Err(e) = state.post(msg).await {
            error!("POST failed: {e}");
            break Err(AcpError::internal_error().data(format!("POST: {e}")));
        }
    };

    state.delete().await;
    for task in sse_tasks {
        task.abort();
    }
    result
}

fn spawn_sse(
    state: Arc<ClientState>,
    session_id: Option<String>,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    tasks.push(tokio::spawn(async move {
        let label = session_id.clone();
        if let Err(e) = state.sse(session_id).await {
            warn!(session_id = ?label, "SSE stream ended: {e}");
        }
    }));
}

struct ClientState {
    endpoint: url::Url,
    http: reqwest::Client,
    connection_id: Mutex<Option<String>>,
    open_session_streams: Mutex<HashSet<String>>,
    incoming: futures::channel::mpsc::UnboundedSender<Result<RawJsonRpcMessage, AcpError>>,
    open_session_tx: futures::channel::mpsc::UnboundedSender<String>,
}

impl ClientState {
    async fn initialize(&self, msg: RawJsonRpcMessage) -> Result<(), String> {
        let response = self
            .http
            .post(self.endpoint.clone())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&msg)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("HTTP {status}: {body}"));
        }

        let connection_id = response
            .headers()
            .get(HEADER_CONNECTION_ID)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
            .ok_or_else(|| format!("server did not return {HEADER_CONNECTION_ID} header"))?;
        let message = response
            .json::<RawJsonRpcMessage>()
            .await
            .map_err(|e| e.to_string())?;

        *self.connection_id.lock().await = Some(connection_id);
        self.deliver(message);
        Ok(())
    }

    async fn post(&self, msg: RawJsonRpcMessage) -> Result<(), String> {
        let session_id = match method_for_message(&msg) {
            Some(method) => {
                let session_id = session_id_from_message(&msg);
                if method_requires_session_header(method) && session_id.is_none() {
                    return Err(format!("method `{method}` requires sessionId in params"));
                }
                session_id
            }
            None => None,
        };
        let connection_id = self
            .connection_id
            .lock()
            .await
            .clone()
            .ok_or_else(|| "POST attempted before initialize".to_string())?;
        let mut request = self
            .http
            .post(self.endpoint.clone())
            .header("Accept", "application/json")
            .header(HEADER_CONNECTION_ID, connection_id)
            .json(&msg);
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id);
        }

        let response = request.send().await.map_err(|e| e.to_string())?;
        if response.status().as_u16() != 202 && !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("HTTP {status}: {body}"));
        }
        Ok(())
    }

    async fn delete(&self) {
        let Some(connection_id) = self.connection_id.lock().await.clone() else {
            return;
        };
        if let Err(e) = self
            .http
            .delete(self.endpoint.clone())
            .header(HEADER_CONNECTION_ID, connection_id)
            .send()
            .await
        {
            debug!("DELETE failed (ignored): {e}");
        }
    }

    async fn sse(&self, session_id: Option<String>) -> Result<(), String> {
        let connection_id = self
            .connection_id
            .lock()
            .await
            .clone()
            .ok_or_else(|| "SSE attempted before initialize".to_string())?;
        let mut request = self
            .http
            .get(self.endpoint.clone())
            .header("Accept", "text/event-stream")
            .header(HEADER_CONNECTION_ID, connection_id);
        if let Some(session_id) = &session_id {
            request = request.header(HEADER_SESSION_ID, session_id);
        }

        let response = request.send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        trace!(session_id = ?session_id, "SSE stream open");

        let mut events = eventsource_stream::EventStream::new(response.bytes_stream());
        while let Some(event) = events.next().await {
            let payload = event.map_err(|e| e.to_string())?.data;
            if payload.is_empty() {
                continue;
            }
            match serde_json::from_str::<RawJsonRpcMessage>(&payload) {
                Ok(msg) => {
                    if let RawJsonRpcMessage::Response(RpcResponse::Result { result, .. }) = &msg
                        && let Some(session_id) = result
                            .get("sessionId")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                        && self
                            .open_session_streams
                            .lock()
                            .await
                            .insert(session_id.clone())
                    {
                        drop(self.open_session_tx.unbounded_send(session_id));
                    }
                    self.deliver(msg);
                }
                Err(e) => warn!("SSE: malformed JSON-RPC payload: {e}"),
            }
        }
        Ok(())
    }

    fn deliver(&self, msg: RawJsonRpcMessage) {
        if self.incoming.unbounded_send(Ok(msg)).is_err() {
            debug!("upstream channel closed; dropping inbound message");
        }
    }
}

async fn run_ws(client: HttpClient, channel: Channel) -> Result<(), AcpError> {
    let HttpClient { endpoint, .. } = client;
    let Channel {
        rx: mut outgoing,
        tx: incoming,
    } = channel;

    let (ws_stream, response) = tokio_tungstenite::connect_async(endpoint.as_str())
        .await
        .map_err(|e| AcpError::internal_error().data(format!("WebSocket connect failed: {e}")))?;
    trace!(
        status = %response.status(),
        "WebSocket connection established"
    );
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    loop {
        tokio::select! {
            msg = outgoing.next() => match msg {
                Some(Ok(msg)) => {
                    let text = match serde_json::to_string(&msg) {
                        Ok(t) => t,
                        Err(e) => {
                            error!("failed to serialize outbound message: {e}");
                            return Err(AcpError::internal_error()
                                .data(format!("serialize: {e}")));
                        }
                    };
                    if let Err(e) = ws_tx.send(WsMessage::Text(text.into())).await {
                        error!("WebSocket send failed: {e}");
                        return Err(AcpError::internal_error()
                            .data(format!("ws send: {e}")));
                    }
                }
                Some(Err(e)) => {
                    error!("upstream channel produced error: {e}");
                    return Err(e);
                }
                None => break,
            },
            frame = ws_rx.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    match serde_json::from_str::<RawJsonRpcMessage>(text.as_str()) {
                        Ok(parsed) => {
                            if incoming.unbounded_send(Ok(parsed)).is_err() {
                                debug!("upstream channel closed; stopping WS reader");
                                break;
                            }
                        }
                        Err(e) => warn!("WS: malformed JSON-RPC payload: {e}"),
                    }
                }
                Some(Ok(WsMessage::Binary(_))) => {
                    warn!("ignoring binary WebSocket frame (ACP uses text)");
                }
                Some(Ok(
                    WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_),
                )) => {}
                Some(Ok(WsMessage::Close(frame))) => {
                    debug!("server closed WebSocket: {frame:?}");
                    break;
                }
                Some(Err(e)) => {
                    error!("WebSocket receive error: {e}");
                    return Err(AcpError::internal_error()
                        .data(format!("ws recv: {e}")));
                }
                None => break,
            },
        }
    }

    drop(ws_tx.send(WsMessage::Close(None)).await);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use agent_client_protocol::schema::RequestId;
    use axum::{
        Json, Router,
        http::{HeaderMap, HeaderValue, StatusCode},
        response::IntoResponse,
        routing::post,
    };
    use serde_json::json;
    use tokio::{net::TcpListener, time::timeout};

    use super::*;

    #[tokio::test]
    async fn post_error_deletes_initialized_connection() {
        let delete_count = Arc::new(AtomicUsize::new(0));
        let delete_count_for_handler = delete_count.clone();
        let app = Router::new().route(
            "/acp",
            post(initialize_response).delete(move || {
                let delete_count = delete_count_for_handler.clone();
                async move {
                    delete_count.fetch_add(1, Ordering::SeqCst);
                    StatusCode::ACCEPTED
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = HttpClient::new(format!("http://{addr}")).unwrap();
        let (mut caller, transport) = Channel::duplex();
        let transport = tokio::spawn(run(client, transport));

        caller
            .tx
            .unbounded_send(Ok(RawJsonRpcMessage::request(
                "initialize".to_string(),
                json!({}),
                RequestId::Number(1),
            )
            .unwrap()))
            .unwrap();
        let init_response = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(init_response, RawJsonRpcMessage::Response(_)));

        caller
            .tx
            .unbounded_send(Ok(RawJsonRpcMessage::request(
                "session/prompt".to_string(),
                json!({}),
                RequestId::Number(2),
            )
            .unwrap()))
            .unwrap();
        let error = timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();

        assert!(error.to_string().contains("POST"));
        assert_eq!(delete_count.load(Ordering::SeqCst), 1);

        server.abort();
    }

    async fn initialize_response() -> impl IntoResponse {
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_CONNECTION_ID, HeaderValue::from_static("conn-1"));
        (
            StatusCode::OK,
            headers,
            Json(RawJsonRpcMessage::response(
                RequestId::Number(1),
                Ok(json!({})),
            )),
        )
    }
}
