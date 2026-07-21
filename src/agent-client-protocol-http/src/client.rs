use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex as StdMutex},
};

use agent_client_protocol::{
    Agent, Channel, Client, ConnectTo, Error as AcpError, RawJsonRpcMessage, TransportFrame,
    schema::v1::{RequestId, Response as RpcResponse},
};
use async_tungstenite::tungstenite::Message as WsMessage;
use futures::{
    Stream, StreamExt,
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
    /// `ServerOptions::path`.
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
    /// `ServerOptions::path`.
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
        let shutdown_tx = channel.tx.clone();
        match futures::future::select(
            std::pin::pin!(client.connect_to(channel)),
            std::pin::pin!(transport),
        )
        .await
        {
            futures::future::Either::Left((result, transport)) => {
                result?;

                // Reject sends from escaped client handles while preserving
                // messages already accepted into the channel, then let the
                // physical transport finish those messages.
                shutdown_tx.close_channel();
                transport.await
            }
            futures::future::Either::Right((result, _)) => result,
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
    let mut outgoing_closed = false;

    let result = loop {
        if outgoing_closed && ordered_posts.is_empty() && response_posts.is_empty() {
            break Ok(());
        }

        let event = {
            let outgoing_next = async {
                if outgoing_closed {
                    futures::future::pending().await
                } else {
                    outgoing.next().await
                }
            }
            .fuse();
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

        let frame = match event {
            HttpLoopEvent::Outgoing(msg) => {
                let Some(frame) = msg else {
                    outgoing_closed = true;
                    continue;
                };
                frame
            }
            HttpLoopEvent::SseEvent(event) => {
                let Some(event) = event else {
                    continue;
                };
                let open_session_id = match &event.frame {
                    TransportFrame::Single(message) => state.session_to_open_for_response(message),
                    TransportFrame::Malformed { .. } | TransportFrame::Batch(_) => None,
                };
                state.deliver_frame(event.frame);
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

        let msg = match frame {
            TransportFrame::Single(message) => message,
            frame @ (TransportFrame::Malformed { .. } | TransportFrame::Batch(_)) => {
                if state.connection.connection_id().is_none() {
                    break Err(AcpError::invalid_request()
                        .data("ACP HTTP transport: first message must be `initialize`"));
                }
                match state.prepare_frame_post(frame) {
                    Ok(post) => response_posts.push(post),
                    Err(error) => {
                        error!("POST failed: {error}");
                        break Err(AcpError::internal_error().data(format!("POST: {error}")));
                    }
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
    Outgoing(Option<TransportFrame>),
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
    frame: TransportFrame,
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
    incoming: futures::channel::mpsc::UnboundedSender<TransportFrame>,
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

    fn is_empty(&self) -> bool {
        self.queued.is_empty() && self.in_flight.is_none()
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

        let connection_id = response
            .headers()
            .get(HEADER_CONNECTION_ID)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        if let Some(connection_id) = &connection_id {
            self.connection.set_connection_id(connection_id.clone());
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("HTTP {status}: {body}"));
        }

        let body = response.text().await.map_err(|error| error.to_string())?;
        let message = match TransportFrame::parse_json(&body) {
            Some(TransportFrame::Single(message)) => message,
            Some(TransportFrame::Malformed { error, .. }) => {
                return Err(format!("invalid initialize response: {error}"));
            }
            Some(TransportFrame::Batch(_)) => {
                return Err("initialize response must not be a JSON-RPC batch".to_string());
            }
            None => return Err("initialize response was ignored as malformed".to_string()),
        };

        if matches!(
            message,
            RawJsonRpcMessage::Response(RpcResponse::Error { .. })
        ) {
            self.deliver(message);
            self.connection.close().await;
            return Ok(InitializeOutcome::Rejected);
        }

        connection_id
            .ok_or_else(|| format!("server did not return {HEADER_CONNECTION_ID} header"))?;
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

    fn prepare_frame_post(&self, frame: TransportFrame) -> Result<PendingPost, String> {
        let connection_id = self
            .connection
            .connection_id()
            .ok_or_else(|| "POST attempted before initialize".to_string())?;
        let body = frame.to_json().map_err(|error| error.to_string())?;
        let request = self
            .connection
            .post()
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(HEADER_CONNECTION_ID, connection_id)
            .body(body);
        let response = async move {
            let response = request.send().await.map_err(|error| error.to_string())?;
            if response.status().as_u16() != 202 && !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(format!("HTTP {status}: {body}"));
            }
            Ok(())
        };
        Ok(PendingPost {
            pending_request: None,
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
        self.deliver_frame(TransportFrame::Single(msg));
    }

    fn deliver_frame(&self, frame: TransportFrame) {
        if self.incoming.unbounded_send(frame).is_err() {
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
        let Some(frame) = TransportFrame::parse_json(&payload) else {
            debug!("ignoring malformed response-shaped SSE payload");
            continue;
        };

        if event_tx.unbounded_send(SseMessage { frame }).is_err() {
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

    let (ws_stream, response) = async_tungstenite::tokio::connect_async(endpoint.as_str())
        .await
        .map_err(|e| AcpError::internal_error().data(format!("WebSocket connect failed: {e}")))?;
    trace!(
        status = %response.status(),
        "WebSocket connection established"
    );
    let (ws_tx, ws_rx) = ws_stream.split();

    drive_ws(ws_tx, ws_rx, channel).await
}

trait WsSink {
    fn send(
        &mut self,
        message: WsMessage,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

impl<S> WsSink for async_tungstenite::WebSocketSender<S>
where
    S: futures::AsyncRead + futures::AsyncWrite + Unpin + Send,
{
    async fn send(&mut self, message: WsMessage) -> Result<(), String> {
        async_tungstenite::WebSocketSender::send(self, message)
            .await
            .map_err(|error| error.to_string())
    }
}

async fn drive_ws<Tx, Rx, RxError>(
    mut ws_tx: Tx,
    mut ws_rx: Rx,
    channel: Channel,
) -> Result<(), AcpError>
where
    Tx: WsSink,
    Rx: Stream<Item = Result<WsMessage, RxError>> + Unpin,
    RxError: std::fmt::Display,
{
    let Channel {
        rx: mut outgoing,
        tx: incoming,
    } = channel;
    let writer = async move {
        while let Some(frame) = outgoing.next().await {
            let text = match frame.to_json() {
                Ok(text) => text,
                Err(error) => {
                    error!("failed to serialize outbound frame: {error}");
                    return Err(AcpError::internal_error().data(format!("serialize: {error}")));
                }
            };
            if let Err(error) = ws_tx.send(WsMessage::Text(text.into())).await {
                error!("WebSocket send failed: {error}");
                return Err(AcpError::internal_error().data(format!("ws send: {error}")));
            }
        }

        drop(ws_tx.send(WsMessage::Close(None)).await);
        Ok(())
    };

    let reader = async move {
        let mut discard_incoming = false;
        loop {
            match ws_rx.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    if discard_incoming {
                        continue;
                    }
                    let Some(frame) = TransportFrame::parse_json(text.as_str()) else {
                        debug!("ignoring malformed response-shaped WebSocket payload");
                        continue;
                    };
                    if incoming.unbounded_send(frame).is_err() {
                        debug!(
                            "upstream channel closed; discarding WS input while draining output"
                        );
                        discard_incoming = true;
                    }
                }
                Some(Ok(WsMessage::Binary(_))) => {
                    warn!("ignoring binary WebSocket frame (ACP uses text)");
                }
                Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_))) => {}
                Some(Ok(WsMessage::Close(frame))) => {
                    debug!("server closed WebSocket: {frame:?}");
                    return Err(AcpError::internal_error()
                        .data(format!("WebSocket closed by peer: {frame:?}")));
                }
                Some(Err(e)) => {
                    error!("WebSocket receive error: {e}");
                    return Err(AcpError::internal_error().data(format!("ws recv: {e}")));
                }
                None => {
                    return Err(AcpError::internal_error().data("WebSocket stream ended"));
                }
            }
        }
    };

    pin_mut!(writer, reader);
    match futures::future::select(writer, reader).await {
        futures::future::Either::Left((result, _))
        | futures::future::Either::Right((result, _)) => result,
    }
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

    use agent_client_protocol::{TransportBatch, schema::v1::RequestId};
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

    struct PostsThenExitClient {
        finish: Arc<Notify>,
        finished: Arc<Notify>,
        escaped_tx: futures::channel::oneshot::Sender<
            futures::channel::mpsc::UnboundedSender<TransportFrame>,
        >,
    }

    struct QueueOutgoingThenText {
        text: Option<WsMessage>,
        outgoing: Option<mpsc::UnboundedSender<TransportFrame>>,
    }

    struct RecordingWsSink(mpsc::UnboundedSender<WsMessage>);

    struct BackpressuredWsSink {
        output: mpsc::UnboundedSender<WsMessage>,
        started: mpsc::UnboundedSender<()>,
        release: Option<futures::channel::oneshot::Receiver<()>>,
    }

    struct ReleaseBackpressureOnPoll {
        started: mpsc::UnboundedReceiver<()>,
        release: Option<futures::channel::oneshot::Sender<()>>,
    }

    fn single_frame(message: RawJsonRpcMessage) -> TransportFrame {
        TransportFrame::Single(message)
    }

    fn into_single_message(frame: TransportFrame) -> Result<RawJsonRpcMessage, AcpError> {
        match frame {
            TransportFrame::Single(message) => Ok(message),
            TransportFrame::Malformed { error, .. } => Err(error),
            TransportFrame::Batch(_) => {
                Err(AcpError::internal_error().data("expected one JSON-RPC message"))
            }
        }
    }

    trait TransportFrameTestExt {
        fn unwrap(self) -> RawJsonRpcMessage;
    }

    impl TransportFrameTestExt for TransportFrame {
        fn unwrap(self) -> RawJsonRpcMessage {
            into_single_message(self).unwrap()
        }
    }

    impl WsSink for RecordingWsSink {
        async fn send(&mut self, message: WsMessage) -> Result<(), String> {
            self.0
                .unbounded_send(message)
                .map_err(|error| error.to_string())
        }
    }

    impl WsSink for BackpressuredWsSink {
        async fn send(&mut self, message: WsMessage) -> Result<(), String> {
            self.output
                .unbounded_send(message)
                .map_err(|error| error.to_string())?;
            if let Some(release) = self.release.take() {
                self.started
                    .unbounded_send(())
                    .map_err(|error| error.to_string())?;
                release
                    .await
                    .map_err(|_| "mock WebSocket reader did not release send".to_string())?;
            }
            Ok(())
        }
    }

    impl Stream for QueueOutgoingThenText {
        type Item = Result<WsMessage, std::io::Error>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            // Make input ready immediately after queueing output. If the output
            // branch was polled first it was still empty, so either poll order
            // deterministically selects this input frame first.
            if let Some(outgoing) = self.outgoing.take() {
                for method in ["custom/first", "custom/second"] {
                    outgoing
                        .unbounded_send(single_frame(
                            RawJsonRpcMessage::notification(method.to_string(), json!({})).unwrap(),
                        ))
                        .unwrap();
                }
            }
            if let Some(text) = self.text.take() {
                return std::task::Poll::Ready(Some(Ok(text)));
            }
            std::task::Poll::Pending
        }
    }

    impl Stream for ReleaseBackpressureOnPoll {
        type Item = Result<WsMessage, std::io::Error>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            if let std::task::Poll::Ready(Some(())) =
                std::pin::Pin::new(&mut self.started).poll_next(cx)
                && let Some(release) = self.release.take()
            {
                let _result = release.send(());
            }
            std::task::Poll::Pending
        }
    }

    impl ConnectTo<Agent> for PostsThenExitClient {
        async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), AcpError> {
            let Self {
                finish,
                finished,
                escaped_tx,
            } = self;
            let (mut channel, transport) = agent.into_channel_and_future();
            let client = async move {
                escaped_tx.send(channel.tx.clone()).map_err(|_| {
                    AcpError::internal_error().data("escaped sender observer dropped")
                })?;
                channel
                    .tx
                    .unbounded_send(single_frame(
                        RawJsonRpcMessage::request(
                            "initialize".to_string(),
                            json!({}),
                            RequestId::Number(1),
                        )
                        .unwrap(),
                    ))
                    .map_err(|e| {
                        AcpError::internal_error().data(format!("send initialize: {e}"))
                    })?;
                into_single_message(channel.rx.next().await.ok_or_else(|| {
                    AcpError::internal_error().data("initialize response channel closed")
                })?)?;

                for method in ["custom/first", "custom/second"] {
                    channel
                        .tx
                        .unbounded_send(single_frame(
                            RawJsonRpcMessage::notification(method.to_string(), json!({})).unwrap(),
                        ))
                        .map_err(|e| {
                            AcpError::internal_error().data(format!("send {method}: {e}"))
                        })?;
                }

                finish.notified().await;
                finished.notify_one();
                Ok(())
            };

            let ((), ()) = futures::try_join!(transport, client)?;
            Ok(())
        }
    }

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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
            .unwrap();
        let init_response = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(init_response, RawJsonRpcMessage::Response(_)));

        caller
            .tx
            .unbounded_send(single_frame(
                RawJsonRpcMessage::notification(
                    "$/cancel_request".to_string(),
                    json!({
                        "requestId": 2,
                        "sessionId": "session-1"
                    }),
                )
                .unwrap(),
            ))
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
    async fn http_preserves_batch_frames_across_post_and_sse() {
        let (post_tx, mut post_rx) = tokio::sync::mpsc::unbounded_channel();
        let post_count = Arc::new(AtomicUsize::new(0));
        let emit_sse = Arc::new(Notify::new());
        let inbound_batch = json!([
            {
                "jsonrpc": "2.0",
                "method": "custom/inbound-one",
                "params": {}
            },
            {
                "jsonrpc": "2.0",
                "method": "custom/inbound-two",
                "params": {}
            }
        ]);
        let app = Router::new().route(
            "/acp",
            post({
                let post_count = post_count.clone();
                move |body: String| {
                    let post_count = post_count.clone();
                    let post_tx = post_tx.clone();
                    async move {
                        if post_count.fetch_add(1, Ordering::SeqCst) == 0 {
                            return initialize_response().await.into_response();
                        }

                        post_tx
                            .send(serde_json::from_str::<serde_json::Value>(&body).unwrap())
                            .unwrap();
                        StatusCode::ACCEPTED.into_response()
                    }
                }
            })
            .get({
                let emit_sse = emit_sse.clone();
                let inbound_batch = inbound_batch.clone();
                move || {
                    let emit_sse = emit_sse.clone();
                    let inbound_batch = inbound_batch.clone();
                    async move {
                        let stream = async_stream::stream! {
                            emit_sse.notified().await;
                            yield Ok::<_, Infallible>(
                                Event::default().data(inbound_batch.to_string()),
                            );
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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
            .unwrap();
        let init_response = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(init_response, RawJsonRpcMessage::Response(_)));

        let outbound_batch = json!([
            {
                "jsonrpc": "2.0",
                "method": "custom/outbound-one",
                "params": {}
            },
            {
                "jsonrpc": "2.0",
                "method": "custom/outbound-two",
                "params": {}
            }
        ]);
        caller
            .tx
            .unbounded_send(TransportFrame::Batch(
                TransportBatch::from_messages([
                    RawJsonRpcMessage::notification("custom/outbound-one".to_string(), json!({}))
                        .unwrap(),
                    RawJsonRpcMessage::notification("custom/outbound-two".to_string(), json!({}))
                        .unwrap(),
                ])
                .unwrap(),
            ))
            .unwrap();

        let posted = timeout(Duration::from_secs(1), post_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(posted, outbound_batch);

        emit_sse.notify_one();
        let inbound = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(&inbound, TransportFrame::Batch(_)));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&inbound.to_json().unwrap()).unwrap(),
            inbound_batch
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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "custom/sessionish".to_string(),
                    json!({}),
                    RequestId::Number(2),
                )
                .unwrap(),
            ))
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
                        let is_connection_stream = session_header.is_none();
                        get_tx.send(session_header).unwrap();

                        let stream = async_stream::stream! {
                            if is_connection_stream {
                                response_ready.notified().await;
                                yield Ok::<_, Infallible>(sse_event(
                                    RawJsonRpcMessage::response(
                                        RequestId::Number(2),
                                        Ok(json!({ "sessionId": "forked-session" })),
                                    ),
                                ));
                            }
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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "session/fork".to_string(),
                    json!({ "sessionId": "source-session" }),
                    RequestId::Number(2),
                )
                .unwrap(),
            ))
            .unwrap();
        let source_sse_header = timeout(Duration::from_secs(1), get_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source_sse_header.as_deref(), Some("source-session"));

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
    async fn client_completion_drains_ordered_posts_in_order() {
        let first_started = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let second_seen = Arc::new(Notify::new());
        let finish_client = Arc::new(Notify::new());
        let client_finished = Arc::new(Notify::new());
        let (escaped_tx, escaped_rx) = futures::channel::oneshot::channel();
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
        let mut connection = tokio::spawn(client.connect_to(PostsThenExitClient {
            finish: finish_client.clone(),
            finished: client_finished.clone(),
            escaped_tx,
        }));
        let escaped = timeout(Duration::from_secs(1), escaped_rx)
            .await
            .unwrap()
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

        finish_client.notify_one();
        timeout(Duration::from_secs(1), client_finished.notified())
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(100), &mut connection)
                .await
                .is_err(),
            "HTTP transport returned before its accepted POSTs completed"
        );
        assert!(
            escaped
                .unbounded_send(single_frame(
                    RawJsonRpcMessage::notification("custom/too-late".to_string(), json!({}),)
                        .unwrap()
                ))
                .is_err(),
            "escaped client sender remained open after client completion"
        );

        release_first.notify_one();
        timeout(Duration::from_secs(1), second_seen.notified())
            .await
            .unwrap();

        timeout(Duration::from_secs(1), connection)
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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "custom/slow".to_string(),
                    json!({}),
                    RequestId::Number(2),
                )
                .unwrap(),
            ))
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
            .unbounded_send(single_frame(RawJsonRpcMessage::response(
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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
            .unwrap();
        let init_response = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(init_response, RawJsonRpcMessage::Response(_)));

        caller
            .tx
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "session/prompt".to_string(),
                    json!({}),
                    RequestId::Number(2),
                )
                .unwrap(),
            ))
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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
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
    async fn malformed_sse_json_is_delivered_and_transport_continues() {
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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
            .unwrap();
        let init_response = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(init_response, RawJsonRpcMessage::Response(_)));

        let frame = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap();

        let TransportFrame::Malformed { raw, error } = frame else {
            panic!("expected malformed frame, got {frame:?}");
        };
        assert_eq!(raw, "{not json");
        assert_eq!(error.code, AcpError::parse_error().code);
        drop(caller);
        timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
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

        let frame = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap();
        let TransportFrame::Malformed { raw, error } = frame else {
            panic!("expected malformed frame, got {frame:?}");
        };
        assert_eq!(raw, "{not json");
        assert_eq!(error.code, AcpError::parse_error().code);

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
    async fn websocket_serializes_batch_as_one_text_frame() {
        let (caller, transport) = Channel::duplex();
        let Channel {
            tx: outgoing,
            rx: incoming,
        } = caller;
        drop(incoming);
        outgoing
            .unbounded_send(TransportFrame::Batch(
                TransportBatch::from_messages([
                    RawJsonRpcMessage::notification("custom/first".to_string(), json!({})).unwrap(),
                    RawJsonRpcMessage::notification("custom/second".to_string(), json!({}))
                        .unwrap(),
                ])
                .unwrap(),
            ))
            .unwrap();
        drop(outgoing);

        let (ws_output_tx, mut ws_output) = mpsc::unbounded();
        timeout(
            Duration::from_secs(1),
            drive_ws(
                RecordingWsSink(ws_output_tx),
                futures::stream::pending::<Result<WsMessage, std::io::Error>>(),
                transport,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let frames = ws_output.by_ref().collect::<Vec<_>>().await;

        let WsMessage::Text(text) = &frames[0] else {
            panic!("batch was not sent as WebSocket text");
        };
        let batch = serde_json::from_str::<serde_json::Value>(text.as_str()).unwrap();
        let entries = batch.as_array().expect("batch should remain an array");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["method"], "custom/first");
        assert_eq!(entries[1]["method"], "custom/second");
        assert!(matches!(frames.get(1), Some(WsMessage::Close(None))));
        assert_eq!(frames.len(), 2);
    }

    #[tokio::test]
    async fn websocket_drain_discards_incoming_after_receiver_closes() {
        let (caller, transport) = Channel::duplex();
        let Channel {
            tx: outgoing,
            rx: incoming,
        } = caller;
        drop(incoming);

        let inbound =
            RawJsonRpcMessage::notification("custom/inbound".to_string(), json!({})).unwrap();
        let inbound = WsMessage::Text(serde_json::to_string(&inbound).unwrap().into());
        let ws_rx = QueueOutgoingThenText {
            text: Some(inbound),
            outgoing: Some(outgoing),
        };
        let (ws_output_tx, mut ws_output) = mpsc::unbounded();
        timeout(
            Duration::from_secs(1),
            drive_ws(RecordingWsSink(ws_output_tx), ws_rx, transport),
        )
        .await
        .unwrap()
        .unwrap();
        let mut frames = Vec::new();
        while let Some(frame) = ws_output.next().await {
            frames.push(frame);
        }

        let messages = frames
            .iter()
            .filter_map(|frame| match frame {
                WsMessage::Text(text) => {
                    Some(serde_json::from_str::<RawJsonRpcMessage>(text.as_str()).unwrap())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let methods = messages
            .iter()
            .filter_map(method_for_message)
            .collect::<Vec<_>>();
        assert_eq!(methods, ["custom/first", "custom/second"]);
        assert!(matches!(frames.last(), Some(WsMessage::Close(None))));
    }

    #[tokio::test]
    async fn websocket_reader_runs_while_send_is_backpressured() {
        let (caller, transport) = Channel::duplex();
        let Channel {
            tx: outgoing,
            rx: incoming,
        } = caller;
        drop(incoming);
        outgoing
            .unbounded_send(single_frame(
                RawJsonRpcMessage::notification("custom/queued".to_string(), json!({})).unwrap(),
            ))
            .unwrap();
        drop(outgoing);

        let (started_tx, started_rx) = mpsc::unbounded();
        let (release_tx, release_rx) = futures::channel::oneshot::channel();
        let (ws_output_tx, mut ws_output) = mpsc::unbounded();
        let ws_tx = BackpressuredWsSink {
            output: ws_output_tx,
            started: started_tx,
            release: Some(release_rx),
        };
        let ws_rx = ReleaseBackpressureOnPoll {
            started: started_rx,
            release: Some(release_tx),
        };

        timeout(Duration::from_secs(1), drive_ws(ws_tx, ws_rx, transport))
            .await
            .expect("WebSocket reader was not polled while its writer was backpressured")
            .unwrap();
        let frames = ws_output.by_ref().collect::<Vec<_>>().await;

        let WsMessage::Text(text) = &frames[0] else {
            panic!("queued message was not sent as WebSocket text");
        };
        let message = serde_json::from_str::<RawJsonRpcMessage>(text.as_str()).unwrap();
        assert_eq!(method_for_message(&message), Some("custom/queued"));
        assert!(matches!(frames.get(1), Some(WsMessage::Close(None))));
        assert_eq!(frames.len(), 2);
    }

    #[tokio::test]
    async fn peer_ws_close_fails_transport() {
        let app = Router::new().route("/acp", get(close_ws));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = HttpClient::new(format!("ws://{addr}")).unwrap();
        let (_caller, transport) = Channel::duplex();
        let transport = tokio::spawn(run(client, transport));

        let error = timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("WebSocket closed by peer"));

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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
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
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
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

    #[tokio::test]
    async fn malformed_initialize_body_with_connection_id_is_deleted() {
        let delete_count = Arc::new(AtomicUsize::new(0));
        let delete_count_for_handler = delete_count.clone();
        let app = Router::new().route(
            "/acp",
            post(malformed_initialize_response).delete(move || {
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
        let (caller, transport) = Channel::duplex();
        let transport = tokio::spawn(run(client, transport));

        caller
            .tx
            .unbounded_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
            .unwrap();
        let error = timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();

        assert!(error.to_string().contains("initialize"));
        wait_for_delete(&delete_count).await;

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

    async fn malformed_initialize_response() -> impl IntoResponse {
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_CONNECTION_ID, HeaderValue::from_static("conn-1"));
        (StatusCode::OK, headers, "{not json")
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

    async fn close_ws(ws: WebSocketUpgrade) -> impl IntoResponse {
        ws.on_upgrade(|mut socket| async move {
            drop(socket.send(AxumWsMessage::Close(None)).await);
        })
    }

    async fn closed_sse() -> StatusCode {
        StatusCode::OK
    }
}
