use std::{
    collections::HashSet,
    sync::{Arc, Mutex as StdMutex},
};

use agent_client_protocol::{
    Agent, Channel, Client, ConnectTo, Error as AcpError, RawJsonRpcMessage,
    schema::Response as RpcResponse,
};
use futures::{
    SinkExt, StreamExt,
    channel::mpsc::{self, UnboundedSender},
    future::BoxFuture,
};
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
    /// Create a client from a base URL and target the standard ACP endpoint.
    ///
    /// If the URL path is empty, `/acp` is used. Otherwise `/acp` is appended
    /// unless the path already ends with `/acp`.
    pub fn new(base_url: impl AsRef<str>) -> Result<Self, HttpClientError> {
        Self::with_client(base_url, reqwest::Client::new())
    }

    /// Create a client that targets the exact endpoint URL.
    ///
    /// Use this when connecting to a server configured with a custom
    /// [`ServerOptions::path`](crate::ServerOptions::path).
    pub fn with_endpoint(endpoint: impl AsRef<str>) -> Result<Self, HttpClientError> {
        Self::with_endpoint_and_client(endpoint, reqwest::Client::new())
    }

    /// Create a client with a custom HTTP client and the standard ACP endpoint.
    ///
    /// If the URL path is empty, `/acp` is used. Otherwise `/acp` is appended
    /// unless the path already ends with `/acp`.
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

    /// Create a client with a custom HTTP client and exact endpoint URL.
    ///
    /// Use this when connecting to a server configured with a custom
    /// [`ServerOptions::path`](crate::ServerOptions::path).
    pub fn with_endpoint_and_client(
        endpoint: impl AsRef<str>,
        http: reqwest::Client,
    ) -> Result<Self, HttpClientError> {
        let endpoint = url::Url::parse(endpoint.as_ref())?;
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
    let (open_session_tx, mut open_session_rx) = mpsc::unbounded();
    let (sse_failure_tx, mut sse_failure_rx) = mpsc::unbounded();
    let connection = HttpConnection::new(endpoint, http);
    let state = Arc::new(ClientState {
        connection: connection.clone(),
        open_session_streams: Mutex::new(HashSet::new()),
        incoming,
        open_session_tx,
    });
    let mut lifecycle = HttpTransportLifecycle::new(connection);

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
                lifecycle.spawn_sse(
                    state.clone(),
                    Some(session_id),
                    sse_failure_tx.clone(),
                );
                continue;
            }
            Some(failure) = sse_failure_rx.next() => {
                let scope = failure.session_id.as_deref().unwrap_or("connection");
                error!(session_id = ?failure.session_id, error = %failure.error, "SSE stream ended");
                break Err(AcpError::internal_error()
                    .data(format!("{scope} SSE stream ended: {}", failure.error)));
            }
        };

        if state.connection.connection_id().is_none() {
            if !is_initialize_request(&msg) {
                break Err(AcpError::invalid_request()
                    .data("ACP HTTP transport: first message must be `initialize`"));
            }
            match state.initialize(msg).await {
                Ok(InitializeOutcome::Connected) => {
                    lifecycle.spawn_sse(state.clone(), None, sse_failure_tx.clone());
                }
                Ok(InitializeOutcome::Rejected) => {}
                Err(e) => {
                    error!("initialize failed: {e}");
                    break Err(AcpError::internal_error().data(format!("initialize: {e}")));
                }
            }
            continue;
        }

        if let Some(session_id) = session_id_from_message(&msg)
            && state
                .open_session_streams
                .lock()
                .await
                .insert(session_id.clone())
        {
            lifecycle.spawn_sse(state.clone(), Some(session_id), sse_failure_tx.clone());
        }

        if let Err(e) = state.post(msg).await {
            error!("POST failed: {e}");
            break Err(AcpError::internal_error().data(format!("POST: {e}")));
        }
    };

    lifecycle.close().await;
    result
}

#[derive(Debug)]
struct SseFailure {
    session_id: Option<String>,
    error: String,
}

#[derive(Clone, Debug)]
struct HttpConnection {
    endpoint: url::Url,
    http: reqwest::Client,
    connection_id: Arc<StdMutex<Option<String>>>,
}

impl HttpConnection {
    fn new(endpoint: url::Url, http: reqwest::Client) -> Self {
        Self {
            endpoint,
            http,
            connection_id: Arc::new(StdMutex::new(None)),
        }
    }

    fn post(&self) -> reqwest::RequestBuilder {
        self.http.post(self.endpoint.clone())
    }

    fn get(&self) -> reqwest::RequestBuilder {
        self.http.get(self.endpoint.clone())
    }

    fn set_connection_id(&self, connection_id: String) {
        *self.connection_id.lock().expect("mutex poisoned") = Some(connection_id);
    }

    fn connection_id(&self) -> Option<String> {
        self.connection_id.lock().expect("mutex poisoned").clone()
    }

    fn take_connection_id(&self) -> Option<String> {
        self.connection_id.lock().expect("mutex poisoned").take()
    }

    async fn close(&self) {
        let Some(task) = self.spawn_close_task() else {
            return;
        };
        if let Err(e) = task.await {
            debug!("DELETE task failed (ignored): {e}");
        }
    }

    fn spawn_close(&self) {
        drop(self.spawn_close_task());
    }

    fn spawn_close_task(&self) -> Option<tokio::task::JoinHandle<()>> {
        let connection_id = self.take_connection_id()?;
        let http = self.http.clone();
        let endpoint = self.endpoint.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => Some(handle.spawn(Self::send_close(http, endpoint, connection_id))),
            Err(e) => {
                debug!("failed to spawn HTTP DELETE: {e}");
                None
            }
        }
    }

    async fn send_close(http: reqwest::Client, endpoint: url::Url, connection_id: String) {
        if let Err(e) = http
            .delete(endpoint)
            .header(HEADER_CONNECTION_ID, connection_id)
            .send()
            .await
        {
            debug!("DELETE failed (ignored): {e}");
        }
    }
}

#[derive(Debug)]
struct HttpTransportLifecycle {
    connection: HttpConnection,
    sse_tasks: SseTasks,
}

impl HttpTransportLifecycle {
    fn new(connection: HttpConnection) -> Self {
        Self {
            connection,
            sse_tasks: SseTasks::default(),
        }
    }

    fn spawn_sse(
        &mut self,
        state: Arc<ClientState>,
        session_id: Option<String>,
        failure_tx: UnboundedSender<SseFailure>,
    ) {
        self.sse_tasks
            .push(spawn_sse(state, session_id, failure_tx));
    }

    async fn close(&mut self) {
        self.connection.close().await;
        self.sse_tasks.abort_all();
    }
}

impl Drop for HttpTransportLifecycle {
    fn drop(&mut self) {
        self.sse_tasks.abort_all();
        self.connection.spawn_close();
    }
}

fn spawn_sse(
    state: Arc<ClientState>,
    session_id: Option<String>,
    failure_tx: UnboundedSender<SseFailure>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let label = session_id.clone();
        let error = match state.sse(session_id).await {
            Ok(()) => "SSE stream closed".to_string(),
            Err(e) => e,
        };
        warn!(session_id = ?label, "SSE stream ended: {error}");
        drop(failure_tx.unbounded_send(SseFailure {
            session_id: label,
            error,
        }));
    })
}

#[derive(Debug, Default)]
struct SseTasks {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl SseTasks {
    fn push(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.handles.push(handle);
    }

    fn abort_all(&mut self) {
        for handle in self.handles.drain(..) {
            handle.abort();
        }
    }
}

struct ClientState {
    connection: HttpConnection,
    open_session_streams: Mutex<HashSet<String>>,
    incoming: futures::channel::mpsc::UnboundedSender<Result<RawJsonRpcMessage, AcpError>>,
    open_session_tx: mpsc::UnboundedSender<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InitializeOutcome {
    Connected,
    Rejected,
}

impl ClientState {
    async fn initialize(&self, msg: RawJsonRpcMessage) -> Result<InitializeOutcome, String> {
        let response = self
            .connection
            .post()
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
            .map(String::from);
        let message = response
            .json::<RawJsonRpcMessage>()
            .await
            .map_err(|e| e.to_string())?;

        if matches!(
            message,
            RawJsonRpcMessage::Response(RpcResponse::Error { .. })
        ) {
            self.deliver(message);
            return Ok(InitializeOutcome::Rejected);
        }

        let connection_id = connection_id
            .ok_or_else(|| format!("server did not return {HEADER_CONNECTION_ID} header"))?;

        self.connection.set_connection_id(connection_id);
        self.deliver(message);
        Ok(InitializeOutcome::Connected)
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
            .connection
            .connection_id()
            .ok_or_else(|| "POST attempted before initialize".to_string())?;
        let mut request = self
            .connection
            .post()
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

    async fn sse(&self, session_id: Option<String>) -> Result<(), String> {
        let connection_id = self
            .connection
            .connection_id()
            .ok_or_else(|| "SSE attempted before initialize".to_string())?;
        let mut request = self
            .connection
            .get()
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
                Err(e) => return Err(format!("malformed JSON-RPC payload: {e}")),
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
        convert::Infallible,
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
        response::{IntoResponse, Sse, sse::Event},
        routing::post,
    };
    use serde_json::json;
    use tokio::{
        net::TcpListener,
        time::{sleep, timeout},
    };

    use super::*;

    #[test]
    fn new_targets_standard_acp_endpoint() {
        assert_eq!(
            HttpClient::new("http://example.com")
                .unwrap()
                .endpoint
                .as_str(),
            "http://example.com/acp"
        );
        assert_eq!(
            HttpClient::new("http://example.com/proxy")
                .unwrap()
                .endpoint
                .as_str(),
            "http://example.com/proxy/acp"
        );
        assert_eq!(
            HttpClient::new("http://example.com/proxy/acp")
                .unwrap()
                .endpoint
                .as_str(),
            "http://example.com/proxy/acp"
        );
    }

    #[test]
    fn with_endpoint_preserves_explicit_endpoint_path() {
        assert_eq!(
            HttpClient::with_endpoint("http://example.com/agent")
                .unwrap()
                .endpoint
                .as_str(),
            "http://example.com/agent"
        );
        assert_eq!(
            HttpClient::with_endpoint_and_client(
                "ws://example.com/custom/acp?token=abc",
                reqwest::Client::new(),
            )
            .unwrap()
            .endpoint
            .as_str(),
            "ws://example.com/custom/acp?token=abc"
        );
    }

    #[tokio::test]
    async fn post_error_deletes_initialized_connection() {
        let delete_count = Arc::new(AtomicUsize::new(0));
        let delete_count_for_handler = delete_count.clone();
        let app = Router::new().route(
            "/acp",
            post(initialize_response).get(pending_sse).delete(move || {
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

    #[tokio::test]
    async fn connection_sse_disconnect_fails_transport() {
        let delete_count = Arc::new(AtomicUsize::new(0));
        let delete_count_for_handler = delete_count.clone();
        let app = Router::new().route(
            "/acp",
            post(initialize_response).get(closed_sse).delete(move || {
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

        let error = timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();

        assert!(error.to_string().contains("SSE"));
        assert_eq!(delete_count.load(Ordering::SeqCst), 1);

        server.abort();
    }

    #[tokio::test]
    async fn malformed_sse_json_fails_transport() {
        let delete_count = Arc::new(AtomicUsize::new(0));
        let delete_count_for_handler = delete_count.clone();
        let app = Router::new().route(
            "/acp",
            post(initialize_response)
                .get(malformed_sse)
                .delete(move || {
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

        let error = timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();

        assert!(error.to_string().contains("malformed JSON-RPC payload"));
        assert_eq!(delete_count.load(Ordering::SeqCst), 1);

        server.abort();
    }

    #[tokio::test]
    async fn dropped_transport_future_deletes_initialized_connection() {
        let delete_count = Arc::new(AtomicUsize::new(0));
        let delete_count_for_handler = delete_count.clone();
        let app = Router::new().route(
            "/acp",
            post(initialize_response).get(pending_sse).delete(move || {
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
        let mut transport = Box::pin(run(client, transport));

        caller
            .tx
            .unbounded_send(Ok(RawJsonRpcMessage::request(
                "initialize".to_string(),
                json!({}),
                RequestId::Number(1),
            )
            .unwrap()))
            .unwrap();
        let init_response = timeout(Duration::from_secs(1), async {
            tokio::select! {
                result = &mut transport => {
                    panic!("transport ended before initialize response: {result:?}");
                }
                msg = caller.rx.next() => {
                    msg.unwrap().unwrap()
                }
            }
        })
        .await
        .unwrap();
        assert!(matches!(init_response, RawJsonRpcMessage::Response(_)));

        drop(transport);
        wait_for_delete(&delete_count).await;

        server.abort();
    }

    #[tokio::test]
    async fn initialize_error_without_connection_id_is_delivered_without_sse() {
        let get_count = Arc::new(AtomicUsize::new(0));
        let get_count_for_handler = get_count.clone();
        let app = Router::new().route(
            "/acp",
            post(initialize_error_response).get(move || {
                let get_count = get_count_for_handler.clone();
                async move {
                    get_count.fetch_add(1, Ordering::SeqCst);
                    pending_sse().await
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

        assert!(matches!(
            init_response,
            RawJsonRpcMessage::Response(RpcResponse::Error {
                id: RequestId::Number(1),
                ..
            })
        ));
        assert_eq!(get_count.load(Ordering::SeqCst), 0);

        drop(caller);
        timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        server.abort();
    }

    async fn wait_for_delete(delete_count: &AtomicUsize) {
        timeout(Duration::from_secs(1), async {
            loop {
                if delete_count.load(Ordering::SeqCst) > 0 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(delete_count.load(Ordering::SeqCst), 1);
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

    async fn initialize_error_response() -> Json<RawJsonRpcMessage> {
        Json(RawJsonRpcMessage::response(
            RequestId::Number(1),
            Err(AcpError::invalid_request().data("initialize rejected")),
        ))
    }

    async fn pending_sse() -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
        Sse::new(futures::stream::pending())
    }

    async fn malformed_sse() -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
        let invalid = futures::stream::once(async {
            Ok::<_, Infallible>(Event::default().data("{not json"))
        });
        Sse::new(invalid.chain(futures::stream::pending()))
    }

    async fn closed_sse() -> StatusCode {
        StatusCode::OK
    }
}
