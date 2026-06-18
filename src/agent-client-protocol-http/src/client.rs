use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex as StdMutex},
};

use agent_client_protocol::{
    Agent, Channel, Client, ConnectTo, Error as AcpError, RawJsonRpcMessage,
    schema::{RequestId, Response as RpcResponse},
};
use async_tungstenite::tungstenite::Message as WsMessage;
use futures::{
    StreamExt,
    channel::mpsc::{self, UnboundedSender},
    future::{BoxFuture, FutureExt},
    pin_mut,
    stream::FuturesUnordered,
};
use thiserror::Error;
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
    let (sse_event_tx, mut sse_event_rx) = mpsc::unbounded::<SseMessage>();
    let connection = HttpConnection::new(endpoint, http);
    let mut state = ClientState {
        connection: connection.clone(),
        open_session_streams: HashSet::new(),
        pending_requests: HashMap::new(),
        incoming,
    };
    let mut lifecycle = HttpTransportLifecycle::new(connection);
    let mut ordered_posts = PostQueue::default();
    let mut response_posts = PostQueue::default();

    let result = loop {
        let event = {
            let outgoing_next = outgoing.next().fuse();
            let sse_event_next = sse_event_rx.next().fuse();
            let sse_failure_next = lifecycle.next_sse_failure().fuse();
            let ordered_post_next = ordered_posts.next_completion().fuse();
            let response_post_next = response_posts.next_completion().fuse();
            pin_mut!(
                outgoing_next,
                sse_event_next,
                sse_failure_next,
                ordered_post_next,
                response_post_next
            );

            futures::select! {
                msg = outgoing_next => HttpLoopEvent::Outgoing(msg),
                event = sse_event_next => HttpLoopEvent::SseEvent(event),
                failure = sse_failure_next => HttpLoopEvent::SseFailure(failure),
                post = ordered_post_next => HttpLoopEvent::Post(post),
                post = response_post_next => HttpLoopEvent::Post(post),
            }
        };

        let msg = match event {
            HttpLoopEvent::Outgoing(msg) => match msg {
                Some(Ok(msg)) => msg,
                Some(Err(e)) => {
                    error!("upstream channel produced error: {e}");
                    break Err(e);
                }
                None => break Ok(()),
            },
            HttpLoopEvent::SseEvent(event) => {
                let Some(event) = event else {
                    continue;
                };
                let open_session_id = state.session_to_open_for_response(&event.message);
                state.deliver(event.message);
                if let Some(session_id) = open_session_id {
                    lifecycle.start_sse(Some(session_id), sse_event_tx.clone());
                }
                continue;
            }
            HttpLoopEvent::SseFailure(failure) => {
                let scope = failure.session_id.as_deref().unwrap_or("connection");
                error!(session_id = ?failure.session_id, error = %failure.error, "SSE stream ended");
                break Err(AcpError::internal_error()
                    .data(format!("{scope} SSE stream ended: {}", failure.error)));
            }
            HttpLoopEvent::Post(completed) => {
                let CompletedPost {
                    pending_request,
                    result,
                } = completed;
                if let Err(e) = result {
                    state.remove_pending_request(pending_request.as_ref());
                    error!("POST failed: {e}");
                    break Err(AcpError::internal_error().data(format!("POST: {e}")));
                }
                continue;
            }
        };

        if state.connection.connection_id().is_none() {
            if !is_initialize_request(&msg) {
                break Err(AcpError::invalid_request()
                    .data("ACP HTTP transport: first message must be `initialize`"));
            }
            match state.initialize(msg).await {
                Ok(InitializeOutcome::Connected) => {
                    lifecycle.start_sse(None, sse_event_tx.clone());
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
            && state.open_session_streams.insert(session_id.clone())
        {
            lifecycle.start_sse(Some(session_id), sse_event_tx.clone());
        }

        let is_response = matches!(msg, RawJsonRpcMessage::Response(_));
        match state.prepare_post(msg) {
            // Responses answer SSE-delivered callbacks and must not be blocked
            // behind a POST that may be waiting for that callback response.
            Ok(post) if is_response => response_posts.push(post),
            Ok(post) => ordered_posts.push(post),
            Err(e) => {
                error!("POST failed: {e}");
                break Err(AcpError::internal_error().data(format!("POST: {e}")));
            }
        }
    };

    lifecycle.close().await;
    result
}

enum HttpLoopEvent {
    Outgoing(Option<Result<RawJsonRpcMessage, AcpError>>),
    SseEvent(Option<SseMessage>),
    SseFailure(SseFailure),
    Post(CompletedPost),
}

#[derive(Debug)]
struct SseFailure {
    session_id: Option<String>,
    error: String,
}

#[derive(Debug)]
struct SseMessage {
    message: RawJsonRpcMessage,
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

    fn clear_connection_id(&self, expected: &str) {
        let mut connection_id = self.connection_id.lock().expect("mutex poisoned");
        if connection_id.as_deref() == Some(expected) {
            *connection_id = None;
        }
    }

    async fn close(&self) {
        let Some(connection_id) = self.connection_id() else {
            return;
        };
        Self::send_close(
            self.http.clone(),
            self.endpoint.clone(),
            connection_id.clone(),
        )
        .await;
        self.clear_connection_id(&connection_id);
    }

    fn spawn_close(&self) {
        let Some(connection_id) = self.take_connection_id() else {
            return;
        };
        let http = self.http.clone();
        let endpoint = self.endpoint.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                drop(handle.spawn(Self::send_close(http, endpoint, connection_id)));
            }
            Err(e) => {
                debug!("failed to spawn HTTP DELETE: {e}");
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

    fn start_sse(&mut self, session_id: Option<String>, event_tx: UnboundedSender<SseMessage>) {
        self.sse_tasks
            .push(run_sse(self.connection.clone(), session_id, event_tx));
    }

    async fn next_sse_failure(&mut self) -> SseFailure {
        self.sse_tasks.next_failure().await
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

fn run_sse(
    connection: HttpConnection,
    session_id: Option<String>,
    event_tx: UnboundedSender<SseMessage>,
) -> BoxFuture<'static, SseFailure> {
    Box::pin(async move {
        let label = session_id.clone();
        let error = match read_sse(connection, session_id, event_tx).await {
            Ok(()) => "SSE stream closed".to_string(),
            Err(e) => e,
        };
        warn!(session_id = ?label, "SSE stream ended: {error}");
        SseFailure {
            session_id: label,
            error,
        }
    })
}

#[derive(Debug, Default)]
struct SseTasks {
    handles: FuturesUnordered<BoxFuture<'static, SseFailure>>,
}

impl SseTasks {
    fn push(&mut self, task: BoxFuture<'static, SseFailure>) {
        self.handles.push(task);
    }

    async fn next_failure(&mut self) -> SseFailure {
        loop {
            if let Some(failure) = self.handles.next().await {
                return failure;
            }
            futures::future::pending::<()>().await;
        }
    }

    fn abort_all(&mut self) {
        self.handles = FuturesUnordered::new();
    }
}

struct ClientState {
    connection: HttpConnection,
    open_session_streams: HashSet<String>,
    pending_requests: HashMap<RequestId, String>,
    incoming: futures::channel::mpsc::UnboundedSender<Result<RawJsonRpcMessage, AcpError>>,
}

struct PendingPost {
    pending_request: Option<(RequestId, String)>,
    response: BoxFuture<'static, Result<(), String>>,
}

impl PendingPost {
    fn into_completion(self) -> BoxFuture<'static, CompletedPost> {
        let Self {
            pending_request,
            response,
        } = self;
        async move {
            CompletedPost {
                pending_request,
                result: response.await,
            }
        }
        .boxed()
    }
}

#[derive(Debug)]
struct CompletedPost {
    pending_request: Option<(RequestId, String)>,
    result: Result<(), String>,
}

#[derive(Default)]
struct PostQueue {
    queued: VecDeque<PendingPost>,
    in_flight: Option<BoxFuture<'static, CompletedPost>>,
}

impl PostQueue {
    fn push(&mut self, post: PendingPost) {
        self.queued.push_back(post);
        self.start_next();
    }

    async fn next_completion(&mut self) -> CompletedPost {
        loop {
            self.start_next();
            if let Some(in_flight) = self.in_flight.as_mut() {
                let completed = in_flight.await;
                self.in_flight = None;
                return completed;
            }
            futures::future::pending::<()>().await;
        }
    }

    fn start_next(&mut self) {
        if self.in_flight.is_none()
            && let Some(post) = self.queued.pop_front()
        {
            self.in_flight = Some(post.into_completion());
        }
    }
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

    fn prepare_post(&mut self, msg: RawJsonRpcMessage) -> Result<PendingPost, String> {
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

        let pending_request = pending_request_for_message(&msg);
        if let Some((id, method)) = &pending_request {
            self.pending_requests.insert(id.clone(), method.clone());
        }

        let response = async move {
            let response = request.send().await.map_err(|e| e.to_string())?;
            if response.status().as_u16() != 202 && !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(format!("HTTP {status}: {body}"));
            }
            Ok(())
        };
        Ok(PendingPost {
            pending_request,
            response: response.boxed(),
        })
    }

    fn remove_pending_request(&mut self, pending_request: Option<&(RequestId, String)>) {
        if let Some((id, _)) = pending_request {
            self.pending_requests.remove(id);
        }
    }

    fn session_to_open_for_response(&mut self, msg: &RawJsonRpcMessage) -> Option<String> {
        let RawJsonRpcMessage::Response(response) = msg else {
            return None;
        };
        let id = msg.response_id().and_then(pending_request_key)?;
        let method = self.pending_requests.remove(&id);

        if !method.as_deref().is_some_and(is_session_opening_method) {
            return None;
        }
        let RpcResponse::Result { result, .. } = response else {
            return None;
        };
        let session_id = result
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(String::from)?;

        if self.open_session_streams.insert(session_id.clone()) {
            Some(session_id)
        } else {
            None
        }
    }

    fn deliver(&self, msg: RawJsonRpcMessage) {
        if self.incoming.unbounded_send(Ok(msg)).is_err() {
            debug!("upstream channel closed; dropping inbound message");
        }
    }
}

fn is_session_opening_method(method: &str) -> bool {
    matches!(method, "session/new" | "session/fork")
}

async fn read_sse(
    connection: HttpConnection,
    session_id: Option<String>,
    event_tx: UnboundedSender<SseMessage>,
) -> Result<(), String> {
    let connection_id = connection
        .connection_id()
        .ok_or_else(|| "SSE attempted before initialize".to_string())?;
    let mut request = connection
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
        let event = event.map_err(|e| e.to_string())?;
        let payload = event.data;
        if payload.is_empty() {
            continue;
        }
        let msg = serde_json::from_str::<RawJsonRpcMessage>(&payload)
            .map_err(|e| format!("malformed JSON-RPC payload: {e}"))?;

        if event_tx
            .unbounded_send(SseMessage { message: msg })
            .is_err()
        {
            return Err("upstream channel closed".to_string());
        }
    }
    Ok(())
}

fn pending_request_for_message(msg: &RawJsonRpcMessage) -> Option<(RequestId, String)> {
    let RawJsonRpcMessage::Request(request) = msg else {
        return None;
    };
    pending_request_key(&request.id).map(|id| (id, request.method.to_string()))
}

fn pending_request_key(id: &RequestId) -> Option<RequestId> {
    match id {
        RequestId::Null => None,
        RequestId::Number(_) | RequestId::Str(_) => Some(id.clone()),
    }
}

async fn run_ws(client: HttpClient, channel: Channel) -> Result<(), AcpError> {
    let HttpClient { endpoint, .. } = client;
    let Channel {
        rx: mut outgoing,
        tx: incoming,
    } = channel;

    let (ws_stream, response) = async_tungstenite::tokio::connect_async(endpoint.as_str())
        .await
        .map_err(|e| AcpError::internal_error().data(format!("WebSocket connect failed: {e}")))?;
    trace!(
        status = %response.status(),
        "WebSocket connection established"
    );
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    loop {
        let outgoing_next = outgoing.next().fuse();
        let frame_next = ws_rx.next().fuse();
        pin_mut!(outgoing_next, frame_next);

        futures::select! {
            msg = outgoing_next => match msg {
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
            frame = frame_next => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    match serde_json::from_str::<RawJsonRpcMessage>(text.as_str()) {
                        Ok(parsed) => {
                            if incoming.unbounded_send(Ok(parsed)).is_err() {
                                debug!("upstream channel closed; stopping WS reader");
                                break;
                            }
                        }
                        Err(e) => {
                            let message = format!("malformed JSON-RPC payload: {e}");
                            warn!("WS: {message}");
                            if incoming
                                .unbounded_send(Err(AcpError::parse_error().data(message)))
                                .is_err()
                            {
                                debug!("upstream channel closed; stopping WS reader");
                                break;
                            }
                        }
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
        extract::{WebSocketUpgrade, ws::Message as AxumWsMessage},
        http::{HeaderMap, HeaderValue, StatusCode},
        response::{IntoResponse, Sse, sse::Event},
        routing::{get, post},
    };
    use serde_json::json;
    use tokio::{
        net::TcpListener,
        sync::Notify,
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
    async fn post_sends_cancel_request_without_session_header() {
        let (capture_tx, mut capture_rx) = tokio::sync::mpsc::unbounded_channel();
        let post_count = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/acp",
            post({
                let capture_tx = capture_tx.clone();
                let post_count = post_count.clone();
                move |headers: HeaderMap, Json(message): Json<RawJsonRpcMessage>| {
                    let capture_tx = capture_tx.clone();
                    let post_count = post_count.clone();
                    async move {
                        if post_count.fetch_add(1, Ordering::SeqCst) == 0 {
                            return initialize_response().await.into_response();
                        }

                        capture_tx
                            .send((headers.get(HEADER_SESSION_ID).cloned(), message))
                            .unwrap();
                        StatusCode::ACCEPTED.into_response()
                    }
                }
            })
            .get(pending_sse)
            .delete(|| async { StatusCode::ACCEPTED }),
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
            .unbounded_send(Ok(RawJsonRpcMessage::notification(
                "$/cancel_request".to_string(),
                json!({
                    "requestId": 2,
                    "sessionId": "session-1"
                }),
            )
            .unwrap()))
            .unwrap();

        let (session_header, message) = timeout(Duration::from_secs(1), capture_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(session_header.is_none());
        assert!(matches!(
            message,
            RawJsonRpcMessage::Notification(notification)
                if notification.method.as_ref() == "$/cancel_request"
        ));

        drop(caller);
        timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        server.abort();
    }

    #[tokio::test]
    async fn custom_response_with_session_id_does_not_open_session_sse() {
        let (get_tx, mut get_rx) = tokio::sync::mpsc::unbounded_channel();
        let response_ready = Arc::new(tokio::sync::Notify::new());
        let post_count = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/acp",
            post({
                let post_count = post_count.clone();
                let response_ready = response_ready.clone();
                move |Json(_message): Json<RawJsonRpcMessage>| {
                    let post_count = post_count.clone();
                    let response_ready = response_ready.clone();
                    async move {
                        if post_count.fetch_add(1, Ordering::SeqCst) == 0 {
                            return initialize_response().await.into_response();
                        }

                        response_ready.notify_waiters();
                        StatusCode::ACCEPTED.into_response()
                    }
                }
            })
            .get({
                let get_tx = get_tx.clone();
                let response_ready = response_ready.clone();
                move |headers: HeaderMap| {
                    let get_tx = get_tx.clone();
                    let response_ready = response_ready.clone();
                    async move {
                        let session_header = headers
                            .get(HEADER_SESSION_ID)
                            .and_then(|value| value.to_str().ok())
                            .map(String::from);
                        get_tx.send(session_header).unwrap();

                        let stream = async_stream::stream! {
                            response_ready.notified().await;
                            yield Ok::<_, Infallible>(sse_event(
                                RawJsonRpcMessage::response(
                                    RequestId::Number(2),
                                    Ok(json!({ "sessionId": "session-1" })),
                                ),
                            ));
                            futures::future::pending::<()>().await;
                        };
                        Sse::new(stream)
                    }
                }
            })
            .delete(|| async { StatusCode::ACCEPTED }),
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

        let connection_sse_header = timeout(Duration::from_secs(1), get_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(connection_sse_header.is_none());

        caller
            .tx
            .unbounded_send(Ok(RawJsonRpcMessage::request(
                "custom/sessionish".to_string(),
                json!({}),
                RequestId::Number(2),
            )
            .unwrap()))
            .unwrap();
        let response = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            response,
            RawJsonRpcMessage::Response(RpcResponse::Result {
                id: RequestId::Number(2),
                ..
            })
        ));

        assert!(
            timeout(Duration::from_millis(100), get_rx.recv())
                .await
                .is_err(),
            "custom response must not open a session SSE stream"
        );

        drop(caller);
        timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        server.abort();
    }

    #[tokio::test]
    async fn fork_response_with_session_id_opens_session_sse() {
        let (get_tx, mut get_rx) = tokio::sync::mpsc::unbounded_channel();
        let response_ready = Arc::new(tokio::sync::Notify::new());
        let post_count = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/acp",
            post({
                let post_count = post_count.clone();
                let response_ready = response_ready.clone();
                move |Json(_message): Json<RawJsonRpcMessage>| {
                    let post_count = post_count.clone();
                    let response_ready = response_ready.clone();
                    async move {
                        if post_count.fetch_add(1, Ordering::SeqCst) == 0 {
                            return initialize_response().await.into_response();
                        }

                        response_ready.notify_waiters();
                        StatusCode::ACCEPTED.into_response()
                    }
                }
            })
            .get({
                let get_tx = get_tx.clone();
                let response_ready = response_ready.clone();
                move |headers: HeaderMap| {
                    let get_tx = get_tx.clone();
                    let response_ready = response_ready.clone();
                    async move {
                        let session_header = headers
                            .get(HEADER_SESSION_ID)
                            .and_then(|value| value.to_str().ok())
                            .map(String::from);
                        get_tx.send(session_header).unwrap();

                        let stream = async_stream::stream! {
                            response_ready.notified().await;
                            yield Ok::<_, Infallible>(sse_event(
                                RawJsonRpcMessage::response(
                                    RequestId::Number(2),
                                    Ok(json!({ "sessionId": "forked-session" })),
                                ),
                            ));
                            futures::future::pending::<()>().await;
                        };
                        Sse::new(stream)
                    }
                }
            })
            .delete(|| async { StatusCode::ACCEPTED }),
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

        let connection_sse_header = timeout(Duration::from_secs(1), get_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(connection_sse_header.is_none());

        caller
            .tx
            .unbounded_send(Ok(RawJsonRpcMessage::request(
                "session/fork".to_string(),
                json!({}),
                RequestId::Number(2),
            )
            .unwrap()))
            .unwrap();
        let response = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            response,
            RawJsonRpcMessage::Response(RpcResponse::Result {
                id: RequestId::Number(2),
                ..
            })
        ));

        let fork_sse_header = timeout(Duration::from_secs(1), get_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fork_sse_header.as_deref(), Some("forked-session"));

        drop(caller);
        timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        server.abort();
    }

    #[tokio::test]
    async fn outbound_posts_are_sent_in_order() {
        let first_started = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let second_seen = Arc::new(Notify::new());
        let app = Router::new().route(
            "/acp",
            post({
                let first_started = first_started.clone();
                let release_first = release_first.clone();
                let second_seen = second_seen.clone();
                move |Json(message): Json<RawJsonRpcMessage>| {
                    let first_started = first_started.clone();
                    let release_first = release_first.clone();
                    let second_seen = second_seen.clone();
                    async move {
                        if is_initialize_request(&message) {
                            return initialize_response().await.into_response();
                        }

                        match method_for_message(&message) {
                            Some("custom/first") => {
                                first_started.notify_one();
                                release_first.notified().await;
                            }
                            Some("custom/second") => {
                                second_seen.notify_one();
                            }
                            _ => {}
                        }
                        StatusCode::ACCEPTED.into_response()
                    }
                }
            })
            .get(pending_sse)
            .delete(|| async { StatusCode::ACCEPTED }),
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
            .unbounded_send(Ok(RawJsonRpcMessage::notification(
                "custom/first".to_string(),
                json!({}),
            )
            .unwrap()))
            .unwrap();
        caller
            .tx
            .unbounded_send(Ok(RawJsonRpcMessage::notification(
                "custom/second".to_string(),
                json!({}),
            )
            .unwrap()))
            .unwrap();

        timeout(Duration::from_secs(1), first_started.notified())
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(100), second_seen.notified())
                .await
                .is_err(),
            "second POST must not be sent while the first POST is pending"
        );

        release_first.notify_one();
        timeout(Duration::from_secs(1), second_seen.notified())
            .await
            .unwrap();

        drop(caller);
        timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        server.abort();
    }

    #[tokio::test]
    async fn sse_continues_while_post_is_pending() {
        let post_started = Arc::new(Notify::new());
        let callback_response_seen = Arc::new(Notify::new());
        let sse_started = Arc::new(Notify::new());
        let (callback_tx, mut callback_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new().route(
            "/acp",
            post({
                let post_started = post_started.clone();
                let callback_response_seen = callback_response_seen.clone();
                let callback_tx = callback_tx.clone();
                move |Json(message): Json<RawJsonRpcMessage>| {
                    let post_started = post_started.clone();
                    let callback_response_seen = callback_response_seen.clone();
                    let callback_tx = callback_tx.clone();
                    async move {
                        if is_initialize_request(&message) {
                            return initialize_response().await.into_response();
                        }

                        match &message {
                            RawJsonRpcMessage::Request(request)
                                if request.method.as_ref() == "custom/slow" =>
                            {
                                post_started.notify_waiters();
                                callback_response_seen.notified().await;
                                StatusCode::ACCEPTED.into_response()
                            }
                            RawJsonRpcMessage::Response(
                                RpcResponse::Result {
                                    id: RequestId::Number(99),
                                    ..
                                }
                                | RpcResponse::Error {
                                    id: RequestId::Number(99),
                                    ..
                                },
                            ) => {
                                callback_tx.send(message).unwrap();
                                callback_response_seen.notify_waiters();
                                StatusCode::ACCEPTED.into_response()
                            }
                            _ => StatusCode::ACCEPTED.into_response(),
                        }
                    }
                }
            })
            .get({
                let post_started = post_started.clone();
                let sse_started = sse_started.clone();
                move || {
                    let post_started = post_started.clone();
                    let sse_started = sse_started.clone();
                    async move {
                        let stream = async_stream::stream! {
                            sse_started.notify_waiters();
                            post_started.notified().await;
                            yield Ok::<_, Infallible>(sse_event(
                                RawJsonRpcMessage::request(
                                    "client/callback".to_string(),
                                    json!({}),
                                    RequestId::Number(99),
                                )
                                .unwrap(),
                            ));
                            futures::future::pending::<()>().await;
                        };
                        Sse::new(stream)
                    }
                }
            })
            .delete(|| async { StatusCode::ACCEPTED }),
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
        timeout(Duration::from_secs(1), sse_started.notified())
            .await
            .unwrap();

        caller
            .tx
            .unbounded_send(Ok(RawJsonRpcMessage::request(
                "custom/slow".to_string(),
                json!({}),
                RequestId::Number(2),
            )
            .unwrap()))
            .unwrap();

        let callback = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            callback,
            RawJsonRpcMessage::Request(request)
                if request.method.as_ref() == "client/callback"
                    && request.id == RequestId::Number(99)
        ));

        caller
            .tx
            .unbounded_send(Ok(RawJsonRpcMessage::response(
                RequestId::Number(99),
                Ok(json!({})),
            )))
            .unwrap();
        let callback_response = timeout(Duration::from_secs(1), callback_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            callback_response,
            RawJsonRpcMessage::Response(RpcResponse::Result {
                id: RequestId::Number(99),
                ..
            })
        ));

        drop(caller);
        timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        server.abort();
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
    async fn malformed_ws_json_reports_parse_error_and_continues() {
        let app = Router::new().route("/acp", get(malformed_then_valid_ws));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = HttpClient::new(format!("ws://{addr}")).unwrap();
        let (mut caller, transport) = Channel::duplex();
        let transport = tokio::spawn(run(client, transport));

        let error = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("malformed JSON-RPC payload"));

        let message = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(message, RawJsonRpcMessage::Response(_)));

        drop(caller);
        timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

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
    async fn dropped_transport_during_close_retries_delete() {
        let delete_count = Arc::new(AtomicUsize::new(0));
        let delete_count_for_handler = delete_count.clone();
        let release_delete = Arc::new(Notify::new());
        let release_delete_for_handler = release_delete.clone();
        let app = Router::new().route(
            "/acp",
            post(initialize_response).get(pending_sse).delete(move || {
                let delete_count = delete_count_for_handler.clone();
                let release_delete = release_delete_for_handler.clone();
                async move {
                    delete_count.fetch_add(1, Ordering::SeqCst);
                    release_delete.notified().await;
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

        drop(caller);
        wait_for_delete_count(&delete_count, 1).await;
        transport.abort();
        wait_for_delete_count(&delete_count, 2).await;
        release_delete.notify_waiters();
        drop(transport.await);

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
        wait_for_delete_count(delete_count, 1).await;
        assert_eq!(delete_count.load(Ordering::SeqCst), 1);
    }

    async fn wait_for_delete_count(delete_count: &AtomicUsize, expected: usize) {
        timeout(Duration::from_secs(1), async {
            loop {
                if delete_count.load(Ordering::SeqCst) >= expected {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
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

    fn sse_event(message: RawJsonRpcMessage) -> Event {
        Event::default().data(serde_json::to_string(&message).unwrap())
    }

    async fn malformed_sse() -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
        let invalid = futures::stream::once(async {
            Ok::<_, Infallible>(Event::default().data("{not json"))
        });
        Sse::new(invalid.chain(futures::stream::pending()))
    }

    async fn malformed_then_valid_ws(ws: WebSocketUpgrade) -> impl IntoResponse {
        ws.on_upgrade(|mut socket| async move {
            drop(socket.send(AxumWsMessage::Text("{not json".into())).await);
            let valid = serde_json::to_string(&RawJsonRpcMessage::response(
                RequestId::Number(1),
                Ok(json!({})),
            ))
            .unwrap();
            drop(socket.send(AxumWsMessage::Text(valid.into())).await);
            futures::future::pending::<()>().await;
        })
    }

    async fn closed_sse() -> StatusCode {
        StatusCode::OK
    }
}
