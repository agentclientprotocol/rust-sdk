use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex as StdMutex},
};

use agent_client_protocol::{
    Agent, BudgetedFrame, Channel, Client, ConnectTo, Error as AcpError, FrameAdmission,
    FramePermit, FrameReceiver, FrameSender, RawJsonRpcMessage, TransportBatchEntry,
    TransportFrame,
    schema::v1::{RequestId, Response as RpcResponse},
};
use async_tungstenite::tungstenite::Message as WsMessage;
use futures::{
    SinkExt, Stream, StreamExt,
    channel::mpsc,
    future::{BoxFuture, FutureExt},
    pin_mut,
    stream::FuturesUnordered,
};
use thiserror::Error;
use tracing::{debug, error, trace, warn};

use crate::protocol::{
    HEADER_CONNECTION_ID, HEADER_SESSION_ID, cancelled_request_id, is_initialize_request,
    is_response_only_shape, method_for_message, method_requires_session_header,
    session_id_from_message,
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

    fn into_channel_and_future(self) -> (Channel, agent_client_protocol::ConnectionDriver) {
        let (caller, transport) = Channel::duplex();
        (
            caller,
            agent_client_protocol::ConnectionDriver::new(run(self, transport)),
        )
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
    let admission = incoming.admission();
    let max_operations = admission.limits().max_queued_frames.max(1);
    let (sse_event_tx, mut sse_event_rx) = mpsc::channel::<SseMessage>(max_operations);
    let connection = HttpConnection::new(endpoint, http);
    let mut state = ClientState {
        connection: connection.clone(),
        open_session_streams: HashSet::new(),
        pending_requests: HashMap::new(),
        pending_request_leases: HashMap::new(),
        incoming,
    };
    let mut lifecycle = HttpTransportLifecycle::new(connection, admission, max_operations);
    let mut posts = PostQueues::default();
    let mut buffered_outgoing = VecDeque::new();
    let mut outgoing_closed = false;

    let result = 'transport: loop {
        if outgoing_closed && buffered_outgoing.is_empty() && posts.is_empty() {
            break Ok(());
        }

        let event = {
            let outgoing_next = async {
                if let Some(frame) = buffered_outgoing.pop_front() {
                    Some(frame)
                } else if outgoing_closed {
                    futures::future::pending().await
                } else {
                    outgoing.next().await
                }
            }
            .fuse();
            let sse_event_next = sse_event_rx.next().fuse();
            let sse_failure_next = lifecycle.next_sse_failure().fuse();
            let ordered_post_next = posts.ordered.next_completion().fuse();
            let response_post_next = posts.responses.next_completion().fuse();
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
                let open_session_ids = state.sessions_to_open_for_responses(event.frame.frame());
                state.deliver_budgeted(event.frame).await?;
                for session_id in open_session_ids {
                    match lifecycle
                        .start_sse(
                            Some(session_id),
                            sse_event_tx.clone(),
                            SseStartContext {
                                events: &mut sse_event_rx,
                                outgoing: &mut outgoing,
                                buffered_outgoing: &mut buffered_outgoing,
                                posts: &mut posts,
                                state: &mut state,
                            },
                        )
                        .await
                    {
                        Ok(SseStartOutcome::Established) => {}
                        Ok(SseStartOutcome::OutgoingClosed)
                            if buffered_outgoing.is_empty() && posts.is_empty() =>
                        {
                            break 'transport Ok(());
                        }
                        Ok(SseStartOutcome::OutgoingClosed) => {
                            break 'transport Err(sse_setup_blocked_output_error());
                        }
                        Err(error) => break 'transport Err(error),
                    }
                }
                continue;
            }
            HttpLoopEvent::SseFailure(failure) => {
                break Err(sse_failure_error(failure));
            }
            HttpLoopEvent::Post(completed) => {
                if let Err(error) = handle_completed_post(&mut state, completed) {
                    break Err(error);
                }
                continue;
            }
        };

        let bypass_ordered =
            is_response_only_frame(frame.frame()) || is_cancellation_frame(frame.frame());
        let (frame, permit) = frame.into_parts();
        let msg = match frame {
            TransportFrame::Single(message) => message,
            frame @ (TransportFrame::Malformed { .. } | TransportFrame::Batch(_)) => {
                if state.connection.connection_id().is_none() {
                    break Err(AcpError::invalid_request()
                        .data("ACP HTTP transport: first message must be `initialize`"));
                }
                match state.prepare_frame_post(frame) {
                    // Response-only batches answer SSE-delivered callbacks and
                    // must not be blocked behind the request they answer.
                    Ok((post, session_ids)) => {
                        state.attach_pending_permits(&post.pending_requests, &permit);
                        for session_id in session_ids {
                            match lifecycle
                                .start_sse(
                                    Some(session_id),
                                    sse_event_tx.clone(),
                                    SseStartContext {
                                        events: &mut sse_event_rx,
                                        outgoing: &mut outgoing,
                                        buffered_outgoing: &mut buffered_outgoing,
                                        posts: &mut posts,
                                        state: &mut state,
                                    },
                                )
                                .await
                            {
                                Ok(SseStartOutcome::Established) => {}
                                Ok(SseStartOutcome::OutgoingClosed) => {
                                    break 'transport Err(sse_setup_blocked_output_error());
                                }
                                Err(error) => break 'transport Err(error),
                            }
                        }
                        if let Err(error) =
                            check_post_capacity(&posts, max_operations, bypass_ordered)
                        {
                            break 'transport Err(error);
                        }
                        if bypass_ordered {
                            posts.responses.push_budgeted(post, permit);
                        } else {
                            posts.ordered.push_budgeted(post, permit);
                        }
                    }
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
                    match lifecycle
                        .start_sse(
                            None,
                            sse_event_tx.clone(),
                            SseStartContext {
                                events: &mut sse_event_rx,
                                outgoing: &mut outgoing,
                                buffered_outgoing: &mut buffered_outgoing,
                                posts: &mut posts,
                                state: &mut state,
                            },
                        )
                        .await
                    {
                        Ok(SseStartOutcome::Established) => {}
                        Ok(SseStartOutcome::OutgoingClosed) if buffered_outgoing.is_empty() => {
                            break 'transport Ok(());
                        }
                        Ok(SseStartOutcome::OutgoingClosed) => {
                            break 'transport Err(sse_setup_blocked_output_error());
                        }
                        Err(error) => break 'transport Err(error),
                    }
                }
                Ok(InitializeOutcome::Rejected) => {}
                Err(e) => {
                    error!("initialize failed: {e}");
                    break Err(AcpError::internal_error().data(format!("initialize: {e}")));
                }
            }
            continue;
        }

        if let Some(session_id) = session_id_from_message(&msg) {
            for session_id in state.register_session_streams([session_id]) {
                match lifecycle
                    .start_sse(
                        Some(session_id),
                        sse_event_tx.clone(),
                        SseStartContext {
                            events: &mut sse_event_rx,
                            outgoing: &mut outgoing,
                            buffered_outgoing: &mut buffered_outgoing,
                            posts: &mut posts,
                            state: &mut state,
                        },
                    )
                    .await
                {
                    Ok(SseStartOutcome::Established) => {}
                    Ok(SseStartOutcome::OutgoingClosed) => {
                        break 'transport Err(sse_setup_blocked_output_error());
                    }
                    Err(error) => break 'transport Err(error),
                }
            }
        }

        if let Err(error) = check_post_capacity(&posts, max_operations, bypass_ordered) {
            break Err(error);
        }
        match state.prepare_post(msg) {
            // Responses and cancellation must not be blocked behind a POST
            // that may itself be waiting for their delivery.
            Ok(post) => {
                state.attach_pending_permits(&post.pending_requests, &permit);
                if bypass_ordered {
                    posts.responses.push_budgeted(post, permit);
                } else {
                    posts.ordered.push_budgeted(post, permit);
                }
            }
            Err(e) => {
                error!("POST failed: {e}");
                break Err(AcpError::internal_error().data(format!("POST: {e}")));
            }
        }
    };

    lifecycle.close().await;
    result
}

fn sse_failure_error(failure: SseFailure) -> AcpError {
    let scope = failure.session_id.as_deref().unwrap_or("connection");
    error!(session_id = ?failure.session_id, error = %failure.error, "SSE stream ended");
    AcpError::internal_error().data(format!("{scope} SSE stream ended: {}", failure.error))
}

fn sse_setup_blocked_output_error() -> AcpError {
    AcpError::internal_error()
        .data("outgoing channel closed while accepted messages awaited SSE stream establishment")
}

fn post_capacity_error() -> AcpError {
    AcpError::internal_error().data("HTTP POST operation capacity exceeded")
}

fn check_post_capacity(
    posts: &PostQueues,
    max_operations: usize,
    bypass_ordered: bool,
) -> Result<(), AcpError> {
    // Keep one operation available for callbacks/cancellation even while the
    // ordered data POST is waiting for exactly such a response.
    let reserved = usize::from(!bypass_ordered && max_operations > 1);
    if posts.len() >= max_operations.saturating_sub(reserved).max(1) {
        Err(post_capacity_error())
    } else {
        Ok(())
    }
}

fn handle_completed_post(
    state: &mut ClientState,
    completed: CompletedPost,
) -> Result<(), AcpError> {
    let CompletedPost {
        pending_requests,
        cancelled_requests,
        result,
    } = completed;
    if let Err(error) = result {
        state.remove_pending_requests(&pending_requests);
        error!("POST failed: {error}");
        Err(AcpError::internal_error().data(format!("POST: {error}")))
    } else {
        for id in cancelled_requests {
            state.cancel_pending_request(&id);
        }
        Ok(())
    }
}

fn queue_response_post(
    state: &mut ClientState,
    posts: &mut PostQueues,
    frame: BudgetedFrame,
) -> Result<(), AcpError> {
    check_post_capacity(
        posts,
        state.incoming.admission().limits().max_queued_frames.max(1),
        true,
    )?;
    let (frame, permit) = frame.into_parts();
    let post = match frame {
        TransportFrame::Single(message) => state.prepare_post(message),
        frame @ (TransportFrame::Malformed { .. } | TransportFrame::Batch(_)) => {
            state.prepare_frame_post(frame).map(|(post, session_ids)| {
                debug_assert!(session_ids.is_empty());
                post
            })
        }
    }
    .map_err(|error| {
        error!("POST failed: {error}");
        AcpError::internal_error().data(format!("POST: {error}"))
    })?;
    state.attach_pending_permits(&post.pending_requests, &permit);
    posts.responses.push_budgeted(post, permit);
    Ok(())
}

fn is_response_only_frame(frame: &TransportFrame) -> bool {
    match frame {
        TransportFrame::Single(RawJsonRpcMessage::Response(_)) => true,
        TransportFrame::Batch(batch) => batch.entries().all(|entry| match entry {
            TransportBatchEntry::Message(RawJsonRpcMessage::Response(_)) => true,
            TransportBatchEntry::Malformed { raw, .. } => is_response_only_shape(raw),
            TransportBatchEntry::Message(
                RawJsonRpcMessage::Request(_) | RawJsonRpcMessage::Notification(_),
            ) => false,
        }),
        TransportFrame::Malformed { raw, .. } => {
            serde_json::from_str(raw).is_ok_and(|value| is_response_only_shape(&value))
        }
        TransportFrame::Single(
            RawJsonRpcMessage::Request(_) | RawJsonRpcMessage::Notification(_),
        ) => false,
    }
}

fn is_cancellation_frame(frame: &TransportFrame) -> bool {
    matches!(
        frame,
        TransportFrame::Single(RawJsonRpcMessage::Notification(message))
            if message.method.as_ref() == "$/cancel_request"
    )
}

enum HttpLoopEvent {
    Outgoing(Option<BudgetedFrame>),
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
    frame: BudgetedFrame,
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
            // A stalled peer must not keep the transport's shutdown (and its
            // retained POST/SSE permits) alive indefinitely.
            .timeout(std::time::Duration::from_secs(2))
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
    admission: FrameAdmission,
    max_tasks: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SseStartOutcome {
    Established,
    OutgoingClosed,
}

struct SseStartContext<'a> {
    events: &'a mut mpsc::Receiver<SseMessage>,
    outgoing: &'a mut FrameReceiver,
    buffered_outgoing: &'a mut VecDeque<BudgetedFrame>,
    posts: &'a mut PostQueues,
    state: &'a mut ClientState,
}

impl HttpTransportLifecycle {
    fn new(connection: HttpConnection, admission: FrameAdmission, max_tasks: usize) -> Self {
        Self {
            connection,
            sse_tasks: SseTasks::default(),
            admission,
            max_tasks,
        }
    }

    async fn start_sse(
        &mut self,
        session_id: Option<String>,
        event_tx: mpsc::Sender<SseMessage>,
        context: SseStartContext<'_>,
    ) -> Result<SseStartOutcome, AcpError> {
        let SseStartContext {
            events,
            outgoing,
            buffered_outgoing,
            posts,
            state,
        } = context;
        let mut establishing = FuturesUnordered::new();
        establishing.push(self.begin_sse(session_id, event_tx.clone())?);

        loop {
            if establishing.is_empty() {
                return Ok(SseStartOutcome::Established);
            }
            let outcome = {
                let failure = self.sse_tasks.next_failure().fuse();
                let established_next = establishing.next().fuse();
                let sse_event_next = events.next().fuse();
                let outgoing_next = outgoing.next().fuse();
                let ordered_post_next = posts.ordered.next_completion().fuse();
                let response_post_next = posts.responses.next_completion().fuse();
                pin_mut!(
                    failure,
                    established_next,
                    sse_event_next,
                    outgoing_next,
                    ordered_post_next,
                    response_post_next
                );
                futures::select_biased! {
                    failure = failure => SseStartWait::Failure(failure),
                    established = established_next => SseStartWait::Established(established),
                    event = sse_event_next => SseStartWait::SseEvent(event),
                    post = response_post_next => SseStartWait::Post(post),
                    post = ordered_post_next => SseStartWait::Post(post),
                    outgoing = outgoing_next => SseStartWait::Outgoing(outgoing),
                }
            };
            match outcome {
                SseStartWait::Established(Some(Ok(()))) => {}
                SseStartWait::Established(Some(Err(_))) => {
                    return Err(sse_failure_error(self.sse_tasks.next_failure().await));
                }
                SseStartWait::Established(None) => {
                    return Ok(SseStartOutcome::Established);
                }
                SseStartWait::Failure(failure) => return Err(sse_failure_error(failure)),
                SseStartWait::SseEvent(Some(event)) => {
                    let open_session_ids =
                        state.sessions_to_open_for_responses(event.frame.frame());
                    state.deliver_budgeted(event.frame).await?;
                    for session_id in open_session_ids {
                        establishing.push(self.begin_sse(Some(session_id), event_tx.clone())?);
                    }
                }
                SseStartWait::SseEvent(None) => {
                    return Err(AcpError::internal_error().data("SSE event channel closed"));
                }
                SseStartWait::Post(completed) => handle_completed_post(state, completed)?,
                SseStartWait::Outgoing(Some(frame))
                    if is_response_only_frame(frame.frame())
                        || is_cancellation_frame(frame.frame()) =>
                {
                    queue_response_post(state, posts, frame)?;
                }
                SseStartWait::Outgoing(Some(frame)) => {
                    if buffered_outgoing.len() + posts.len() >= self.max_tasks {
                        return Err(post_capacity_error());
                    }
                    buffered_outgoing.push_back(frame);
                }
                SseStartWait::Outgoing(None) => return Ok(SseStartOutcome::OutgoingClosed),
            }
        }
    }

    fn begin_sse(
        &mut self,
        session_id: Option<String>,
        event_tx: mpsc::Sender<SseMessage>,
    ) -> Result<futures::channel::oneshot::Receiver<()>, AcpError> {
        if self.sse_tasks.len() >= self.max_tasks {
            return Err(AcpError::internal_error().data("HTTP SSE stream capacity exceeded"));
        }
        let (established_tx, established_rx) = futures::channel::oneshot::channel();
        self.sse_tasks.push(run_sse(
            self.connection.clone(),
            session_id,
            event_tx,
            established_tx,
            self.admission.clone(),
        ));
        Ok(established_rx)
    }

    async fn next_sse_failure(&mut self) -> SseFailure {
        self.sse_tasks.next_failure().await
    }

    async fn close(&mut self) {
        self.connection.close().await;
        self.sse_tasks.abort_all();
    }
}

enum SseStartWait {
    Established(Option<Result<(), futures::channel::oneshot::Canceled>>),
    Failure(SseFailure),
    SseEvent(Option<SseMessage>),
    Post(CompletedPost),
    Outgoing(Option<BudgetedFrame>),
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
    event_tx: mpsc::Sender<SseMessage>,
    established_tx: futures::channel::oneshot::Sender<()>,
    admission: FrameAdmission,
) -> BoxFuture<'static, SseFailure> {
    Box::pin(async move {
        let label = session_id.clone();
        let error =
            match read_sse(connection, session_id, event_tx, established_tx, admission).await {
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
    fn len(&self) -> usize {
        self.handles.len()
    }

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
    pending_requests: HashMap<RequestId, VecDeque<String>>,
    pending_request_leases: HashMap<RequestId, VecDeque<FramePermit>>,
    incoming: FrameSender,
}

struct PendingPost {
    pending_requests: Vec<(RequestId, String)>,
    cancelled_requests: Vec<RequestId>,
    response: BoxFuture<'static, Result<(), String>>,
}

impl PendingPost {
    fn into_completion(self, permit: Option<FramePermit>) -> BoxFuture<'static, CompletedPost> {
        let Self {
            pending_requests,
            cancelled_requests,
            response,
        } = self;
        async move {
            let completed = CompletedPost {
                pending_requests,
                cancelled_requests,
                result: response.await,
            };
            drop(permit);
            completed
        }
        .boxed()
    }
}

#[derive(Debug)]
struct CompletedPost {
    pending_requests: Vec<(RequestId, String)>,
    cancelled_requests: Vec<RequestId>,
    result: Result<(), String>,
}

#[derive(Default)]
struct PostQueue {
    queued: VecDeque<(PendingPost, Option<FramePermit>)>,
    in_flight: Option<BoxFuture<'static, CompletedPost>>,
}

#[derive(Default)]
struct PostQueues {
    ordered: PostQueue,
    responses: PostQueue,
}

impl PostQueues {
    fn is_empty(&self) -> bool {
        self.ordered.is_empty() && self.responses.is_empty()
    }

    fn len(&self) -> usize {
        self.ordered.len() + self.responses.len()
    }
}

impl PostQueue {
    fn len(&self) -> usize {
        self.queued.len() + usize::from(self.in_flight.is_some())
    }

    #[cfg(test)]
    fn push(&mut self, post: PendingPost) {
        self.queued.push_back((post, None));
        self.start_next();
    }

    fn push_budgeted(&mut self, post: PendingPost, permit: FramePermit) {
        self.queued.push_back((post, Some(permit)));
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
            && let Some((post, permit)) = self.queued.pop_front()
        {
            self.in_flight = Some(post.into_completion(permit));
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
            TransportFrame::Single(message) => message,
            TransportFrame::Malformed { error, .. } => {
                return Err(format!("invalid initialize response: {error}"));
            }
            TransportFrame::Batch(_) => {
                return Err("initialize response must not be a JSON-RPC batch".to_string());
            }
        };

        if matches!(
            message,
            RawJsonRpcMessage::Response(RpcResponse::Error { .. })
        ) {
            self.deliver(message).await.map_err(|e| e.to_string())?;
            self.connection.close().await;
            return Ok(InitializeOutcome::Rejected);
        }

        connection_id
            .ok_or_else(|| format!("server did not return {HEADER_CONNECTION_ID} header"))?;
        self.deliver(message).await.map_err(|e| e.to_string())?;
        Ok(InitializeOutcome::Connected)
    }

    fn prepare_post(&mut self, msg: RawJsonRpcMessage) -> Result<PendingPost, String> {
        let session_id = validated_session_id(&msg)?;
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

        let pending_requests = pending_request_for_message(&msg)
            .into_iter()
            .collect::<Vec<_>>();
        let cancelled_requests = cancelled_request_id(&msg).into_iter().collect();
        self.check_pending_request_capacity(pending_requests.len())?;
        self.track_pending_requests(&pending_requests);

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
            pending_requests,
            cancelled_requests,
            response: response.boxed(),
        })
    }

    fn prepare_frame_post(
        &mut self,
        frame: TransportFrame,
    ) -> Result<(PendingPost, Vec<String>), String> {
        let bookkeeping = FrameBookkeeping::for_frame(&frame)?;
        self.check_pending_request_capacity(bookkeeping.pending_requests.len())?;
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
        self.track_pending_requests(&bookkeeping.pending_requests);
        let session_ids = self.register_session_streams(bookkeeping.session_ids);
        Ok((
            PendingPost {
                pending_requests: bookkeeping.pending_requests,
                cancelled_requests: bookkeeping.cancelled_requests,
                response: response.boxed(),
            },
            session_ids,
        ))
    }

    fn track_pending_requests(&mut self, pending_requests: &[(RequestId, String)]) {
        for (id, method) in pending_requests {
            self.pending_requests
                .entry(id.clone())
                .or_default()
                .push_back(method.clone());
        }
    }

    fn check_pending_request_capacity(&self, additional: usize) -> Result<(), String> {
        let limit = self.incoming.admission().limits().max_queued_frames.max(1);
        let existing: usize = self.pending_requests.values().map(VecDeque::len).sum();
        if additional > limit.saturating_sub(existing) {
            Err("HTTP pending request capacity exceeded".to_string())
        } else {
            Ok(())
        }
    }

    fn attach_pending_permits(
        &mut self,
        pending_requests: &[(RequestId, String)],
        permit: &FramePermit,
    ) {
        for (id, _) in pending_requests {
            self.pending_request_leases
                .entry(id.clone())
                .or_default()
                .push_back(permit.clone());
        }
    }

    fn remove_pending_requests(&mut self, pending_requests: &[(RequestId, String)]) {
        for (id, method) in pending_requests.iter().rev() {
            let remove_entry = self.pending_requests.get_mut(id).is_some_and(|methods| {
                if let Some(index) = methods.iter().rposition(|candidate| candidate == method) {
                    methods.remove(index);
                    if let Some(leases) = self.pending_request_leases.get_mut(id) {
                        leases.remove(index);
                    }
                }
                methods.is_empty()
            });
            if remove_entry {
                self.pending_requests.remove(id);
                self.pending_request_leases.remove(id);
            }
        }
    }

    fn take_pending_request_method(&mut self, id: &RequestId) -> Option<String> {
        let (method, remove_entry) = {
            let methods = self.pending_requests.get_mut(id)?;
            (methods.pop_front(), methods.is_empty())
        };
        if let Some(leases) = self.pending_request_leases.get_mut(id) {
            leases.pop_front();
        }
        if remove_entry {
            self.pending_requests.remove(id);
            self.pending_request_leases.remove(id);
        }
        method
    }

    fn cancel_pending_request(&mut self, id: &RequestId) {
        let Some(methods) = self.pending_requests.get_mut(id) else {
            return;
        };
        methods.pop_front();
        if let Some(leases) = self.pending_request_leases.get_mut(id) {
            leases.pop_front();
        }
        if methods.is_empty() {
            self.pending_requests.remove(id);
            self.pending_request_leases.remove(id);
        }
    }

    fn register_session_streams(
        &mut self,
        session_ids: impl IntoIterator<Item = String>,
    ) -> Vec<String> {
        session_ids
            .into_iter()
            .filter(|session_id| self.open_session_streams.insert(session_id.clone()))
            .collect()
    }

    fn sessions_to_open_for_responses(&mut self, frame: &TransportFrame) -> Vec<String> {
        match frame {
            TransportFrame::Single(message) => self
                .session_to_open_for_response(message)
                .into_iter()
                .collect(),
            TransportFrame::Batch(batch) => batch
                .entries()
                .filter_map(|entry| match entry {
                    TransportBatchEntry::Message(message) => {
                        self.session_to_open_for_response(message)
                    }
                    TransportBatchEntry::Malformed { .. } => None,
                })
                .collect(),
            TransportFrame::Malformed { .. } => Vec::new(),
        }
    }

    fn session_to_open_for_response(&mut self, msg: &RawJsonRpcMessage) -> Option<String> {
        let RawJsonRpcMessage::Response(response) = msg else {
            return None;
        };
        let id = msg.response_id().and_then(pending_request_key)?;
        let method = self.take_pending_request_method(&id);

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

    async fn deliver(&self, msg: RawJsonRpcMessage) -> Result<(), AcpError> {
        self.deliver_frame(TransportFrame::Single(msg)).await
    }

    async fn deliver_frame(&self, frame: TransportFrame) -> Result<(), AcpError> {
        self.incoming.send_frame(frame).await
    }

    async fn deliver_budgeted(&self, frame: BudgetedFrame) -> Result<(), AcpError> {
        self.incoming
            .clone()
            .send(frame)
            .await
            .map_err(|error| AcpError::internal_error().data(format!("deliver SSE frame: {error}")))
    }
}

#[derive(Default)]
struct FrameBookkeeping {
    session_ids: Vec<String>,
    pending_requests: Vec<(RequestId, String)>,
    cancelled_requests: Vec<RequestId>,
}

impl FrameBookkeeping {
    fn for_frame(frame: &TransportFrame) -> Result<Self, String> {
        let mut bookkeeping = Self::default();
        match frame {
            TransportFrame::Single(message) => bookkeeping.add_message(message)?,
            TransportFrame::Batch(batch) => {
                for entry in batch.entries() {
                    if let TransportBatchEntry::Message(message) = entry {
                        bookkeeping.add_message(message)?;
                    }
                }
            }
            TransportFrame::Malformed { .. } => {}
        }
        Ok(bookkeeping)
    }

    fn add_message(&mut self, message: &RawJsonRpcMessage) -> Result<(), String> {
        if let Some(session_id) = validated_session_id(message)?
            && !self.session_ids.contains(&session_id)
        {
            self.session_ids.push(session_id);
        }
        if let Some(pending_request) = pending_request_for_message(message) {
            self.pending_requests.push(pending_request);
        }
        self.cancelled_requests
            .extend(cancelled_request_id(message));
        Ok(())
    }
}

fn validated_session_id(msg: &RawJsonRpcMessage) -> Result<Option<String>, String> {
    let Some(method) = method_for_message(msg) else {
        return Ok(None);
    };
    let session_id = session_id_from_message(msg);
    if method_requires_session_header(method) && session_id.is_none() {
        return Err(format!("method `{method}` requires sessionId in params"));
    }
    Ok(session_id)
}

fn is_session_opening_method(method: &str) -> bool {
    matches!(method, "session/new" | "session/fork")
}

async fn read_sse(
    connection: HttpConnection,
    session_id: Option<String>,
    mut event_tx: mpsc::Sender<SseMessage>,
    established_tx: futures::channel::oneshot::Sender<()>,
    admission: FrameAdmission,
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
    let _ = established_tx.send(());

    // Cap each event before EventStream buffers its data fields or JSON parsing
    // materializes the payload. A blank line terminates one SSE event.
    let max_frame_bytes = admission.limits().max_frame_bytes;
    let mut event_bytes = 0usize;
    let mut line_has_data = false;
    let mut events =
        eventsource_stream::EventStream::new(response.bytes_stream().map(move |chunk| {
            let chunk = chunk.map_err(std::io::Error::other)?;
            for &byte in &chunk {
                event_bytes += 1;
                if event_bytes > max_frame_bytes {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "SSE event exceeds maximum JSON-RPC frame size",
                    ));
                }
                if byte == b'\n' {
                    if !line_has_data {
                        event_bytes = 0;
                    }
                    line_has_data = false;
                } else if byte != b'\r' {
                    line_has_data = true;
                }
            }
            Ok(chunk)
        }));
    while let Some(event) = events.next().await {
        let event = event.map_err(|e| e.to_string())?;
        let payload = event.data;
        if payload.is_empty() {
            continue;
        }
        let frame = admission
            .admit(TransportFrame::parse_json(&payload))
            .await
            .map_err(|error| error.to_string())?;

        if event_tx.send(SseMessage { frame }).await.is_err() {
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
            let text = match frame.frame().to_json() {
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
                    let frame = TransportFrame::parse_json(text.as_str());
                    if incoming.send_frame(frame).await.is_err() {
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
#[path = "client_admission_tests.rs"]
mod admission_tests;

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
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
        escaped_tx: futures::channel::oneshot::Sender<FrameSender>,
    }

    struct InitializeThenExitClient {
        sse_started: Arc<Notify>,
        finished: Arc<Notify>,
    }

    struct QueueOutgoingThenText {
        text: Option<WsMessage>,
        outgoing: Option<FrameSender>,
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

    impl TransportFrameTestExt for agent_client_protocol::BudgetedFrame {
        fn unwrap(self) -> RawJsonRpcMessage {
            into_single_message(self.into_frame()).unwrap()
        }
    }

    #[test]
    fn malformed_response_shapes_bypass_only_when_the_whole_frame_is_response_only() {
        let standalone_response = TransportFrame::parse_json(
            r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-32603}}"#,
        );
        assert!(is_response_only_frame(&standalone_response));

        let response_batch = TransportFrame::parse_json(
            r#"[
                {"jsonrpc":"2.0","id":1,"result":{}},
                {"jsonrpc":"2.0","id":2,"result":{},"error":{"code":-32603}}
            ]"#,
        );
        assert!(is_response_only_frame(&response_batch));

        let scalar_batch = TransportFrame::parse_json(
            r#"[
                {"jsonrpc":"2.0","id":1,"result":{}},
                17
            ]"#,
        );
        assert!(!is_response_only_frame(&scalar_batch));

        let call_shaped = TransportFrame::parse_json(
            r#"{"jsonrpc":"2.0","id":1,"method":"custom/call","result":{}}"#,
        );
        assert!(!is_response_only_frame(&call_shaped));
    }

    fn initialized_client_state() -> ClientState {
        let connection = HttpConnection::new(
            url::Url::parse("http://127.0.0.1/acp").unwrap(),
            reqwest::Client::new(),
        );
        connection.set_connection_id("connection-1".to_string());
        let (incoming, _incoming_rx) = Channel::duplex();
        ClientState {
            connection,
            open_session_streams: HashSet::new(),
            pending_requests: HashMap::new(),
            pending_request_leases: HashMap::new(),
            incoming: incoming.tx,
        }
    }

    #[test]
    fn batch_post_validation_happens_before_tracking_requests_or_sessions() {
        let mut state = initialized_client_state();
        let frame = TransportFrame::Batch(
            TransportBatch::from_messages([
                RawJsonRpcMessage::request(
                    "custom/valid".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
                RawJsonRpcMessage::request(
                    "session/prompt".to_string(),
                    json!({ "prompt": [] }),
                    RequestId::Number(2),
                )
                .unwrap(),
            ])
            .unwrap(),
        );

        let Err(error) = state.prepare_frame_post(frame) else {
            panic!("batch should require sessionId for session/prompt");
        };

        assert_eq!(
            error,
            "method `session/prompt` requires sessionId in params"
        );
        assert!(state.pending_requests.is_empty());
        assert!(state.open_session_streams.is_empty());
    }

    #[test]
    fn batch_post_tracks_every_non_null_request_and_rolls_back_from_the_back() {
        let mut state = initialized_client_state();
        state.track_pending_requests(&[(RequestId::Number(7), "session/fork".to_string())]);
        let frame = TransportFrame::Batch(
            TransportBatch::from_messages([
                RawJsonRpcMessage::request(
                    "session/fork".to_string(),
                    json!({ "sessionId": "source-a" }),
                    RequestId::Number(7),
                )
                .unwrap(),
                RawJsonRpcMessage::request(
                    "custom/request".to_string(),
                    json!({ "sessionId": "source-b" }),
                    RequestId::Number(7),
                )
                .unwrap(),
                RawJsonRpcMessage::request(
                    "session/fork".to_string(),
                    json!({ "sessionId": "source-a" }),
                    RequestId::Null,
                )
                .unwrap(),
            ])
            .unwrap(),
        );

        let (post, session_ids) = state.prepare_frame_post(frame).unwrap();

        assert_eq!(session_ids, ["source-a", "source-b"]);
        assert_eq!(
            state.pending_requests.get(&RequestId::Number(7)).unwrap(),
            &VecDeque::from([
                "session/fork".to_string(),
                "session/fork".to_string(),
                "custom/request".to_string(),
            ])
        );
        assert_eq!(
            post.pending_requests,
            [
                (RequestId::Number(7), "session/fork".to_string()),
                (RequestId::Number(7), "custom/request".to_string()),
            ]
        );
        assert!(!state.pending_requests.contains_key(&RequestId::Null));

        state.remove_pending_requests(&post.pending_requests);

        assert_eq!(
            state.pending_requests.get(&RequestId::Number(7)).unwrap(),
            &VecDeque::from(["session/fork".to_string()])
        );
    }

    impl WsSink for RecordingWsSink {
        fn send(
            &mut self,
            message: WsMessage,
        ) -> impl std::future::Future<Output = Result<(), String>> + Send {
            std::future::ready(
                self.0
                    .unbounded_send(message)
                    .map_err(|error| error.to_string()),
            )
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
                        .try_send(single_frame(
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
                    .send_frame(single_frame(
                        RawJsonRpcMessage::request(
                            "initialize".to_string(),
                            json!({}),
                            RequestId::Number(1),
                        )
                        .unwrap(),
                    ))
                    .await
                    .map_err(|e| {
                        AcpError::internal_error().data(format!("send initialize: {e}"))
                    })?;
                into_single_message(
                    channel
                        .rx
                        .next()
                        .await
                        .ok_or_else(|| {
                            AcpError::internal_error().data("initialize response channel closed")
                        })?
                        .into_frame(),
                )?;

                for method in ["custom/first", "custom/second"] {
                    channel
                        .tx
                        .send_frame(single_frame(
                            RawJsonRpcMessage::notification(method.to_string(), json!({})).unwrap(),
                        ))
                        .await
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

    impl ConnectTo<Agent> for InitializeThenExitClient {
        async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), AcpError> {
            let Self {
                sse_started,
                finished,
            } = self;
            let (mut channel, transport) = agent.into_channel_and_future();
            let client = async move {
                channel
                    .tx
                    .send_frame(single_frame(
                        RawJsonRpcMessage::request(
                            "initialize".to_string(),
                            json!({}),
                            RequestId::Number(1),
                        )
                        .unwrap(),
                    ))
                    .await
                    .map_err(|error| {
                        AcpError::internal_error().data(format!("send initialize: {error}"))
                    })?;
                into_single_message(
                    channel
                        .rx
                        .next()
                        .await
                        .ok_or_else(|| {
                            AcpError::internal_error().data("initialize response channel closed")
                        })?
                        .into_frame(),
                )?;

                sse_started.notified().await;
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
            .try_send(single_frame(
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
            .try_send(single_frame(
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
            .try_send(single_frame(
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
            .try_send(TransportFrame::Batch(
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
        assert!(matches!(inbound.frame(), TransportFrame::Batch(_)));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&inbound.frame().to_json().unwrap()).unwrap(),
            inbound_batch
        );
        drop(inbound);

        drop(caller);
        timeout(Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        server.abort();
    }

    #[tokio::test]
    async fn batch_fork_opens_source_and_result_session_streams() {
        let (post_tx, mut post_rx) = tokio::sync::mpsc::unbounded_channel();
        let (get_tx, mut get_rx) = tokio::sync::mpsc::unbounded_channel();
        let post_count = Arc::new(AtomicUsize::new(0));
        let emit_response = Arc::new(Notify::new());
        let connection_stream_established = Arc::new(AtomicBool::new(false));
        let source_stream_established = Arc::new(AtomicBool::new(false));
        let response_batch = json!([
            {
                "jsonrpc": "2.0",
                "id": 2,
                "result": { "sessionId": "forked-session" }
            }
        ]);
        let app = Router::new().route(
            "/acp",
            post({
                let post_count = post_count.clone();
                let connection_stream_established = connection_stream_established.clone();
                let source_stream_established = source_stream_established.clone();
                move |body: String| {
                    let post_count = post_count.clone();
                    let post_tx = post_tx.clone();
                    let connection_stream_established = connection_stream_established.clone();
                    let source_stream_established = source_stream_established.clone();
                    async move {
                        if post_count.fetch_add(1, Ordering::SeqCst) == 0 {
                            return initialize_response().await.into_response();
                        }

                        if !connection_stream_established.load(Ordering::SeqCst)
                            || !source_stream_established.load(Ordering::SeqCst)
                        {
                            return StatusCode::CONFLICT.into_response();
                        }
                        post_tx
                            .send(serde_json::from_str::<serde_json::Value>(&body).unwrap())
                            .unwrap();
                        StatusCode::ACCEPTED.into_response()
                    }
                }
            })
            .get({
                let emit_response = emit_response.clone();
                let response_batch = response_batch.clone();
                let connection_stream_established = connection_stream_established.clone();
                let source_stream_established = source_stream_established.clone();
                move |headers: HeaderMap| {
                    let emit_response = emit_response.clone();
                    let response_batch = response_batch.clone();
                    let get_tx = get_tx.clone();
                    let connection_stream_established = connection_stream_established.clone();
                    let source_stream_established = source_stream_established.clone();
                    async move {
                        let session_id = headers
                            .get(HEADER_SESSION_ID)
                            .and_then(|value| value.to_str().ok())
                            .map(String::from);
                        let is_connection_stream = session_id.is_none();
                        let is_source_stream = session_id.as_deref() == Some("source-session");
                        if is_connection_stream {
                            sleep(Duration::from_millis(50)).await;
                            connection_stream_established.store(true, Ordering::SeqCst);
                        }
                        if is_source_stream {
                            sleep(Duration::from_millis(50)).await;
                            source_stream_established.store(true, Ordering::SeqCst);
                        }
                        get_tx.send(session_id).unwrap();

                        let stream = async_stream::stream! {
                            if is_source_stream {
                                emit_response.notified().await;
                                yield Ok::<_, Infallible>(
                                    Event::default().data(response_batch.to_string()),
                                );
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
            .try_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
            .unwrap();
        timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap();

        caller
            .tx
            .try_send(TransportFrame::Batch(
                TransportBatch::from_messages([RawJsonRpcMessage::request(
                    "session/fork".to_string(),
                    json!({ "sessionId": "source-session" }),
                    RequestId::Number(2),
                )
                .unwrap()])
                .unwrap(),
            ))
            .unwrap();

        let connection_stream = timeout(Duration::from_secs(1), get_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(connection_stream.is_none());
        let source_stream = timeout(Duration::from_secs(1), get_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source_stream.as_deref(), Some("source-session"));
        let posted = timeout(Duration::from_secs(1), post_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(posted.is_array(), "outgoing batch must remain an array");

        emit_response.notify_one();
        let response = timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(response.frame(), TransportFrame::Batch(_)));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response.frame().to_json().unwrap())
                .unwrap(),
            response_batch
        );
        drop(response);
        let forked_stream = timeout(Duration::from_secs(1), get_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(forked_stream.as_deref(), Some("forked-session"));

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
            .try_send(single_frame(
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
            .try_send(single_frame(
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
            .try_send(single_frame(
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
            .try_send(single_frame(
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
    async fn only_response_batches_bypass_ordered_posts() {
        let slow_started = Arc::new(Notify::new());
        let release_slow = Arc::new(Notify::new());
        let call_batch_seen = Arc::new(Notify::new());
        let response_batch_seen = Arc::new(Notify::new());
        let app = Router::new().route(
            "/acp",
            post({
                let slow_started = slow_started.clone();
                let release_slow = release_slow.clone();
                let call_batch_seen = call_batch_seen.clone();
                let response_batch_seen = response_batch_seen.clone();
                move |body: String| {
                    let slow_started = slow_started.clone();
                    let release_slow = release_slow.clone();
                    let call_batch_seen = call_batch_seen.clone();
                    let response_batch_seen = response_batch_seen.clone();
                    async move {
                        let value = serde_json::from_str::<serde_json::Value>(&body).unwrap();
                        if value.get("method").and_then(serde_json::Value::as_str)
                            == Some("initialize")
                        {
                            return initialize_response().await.into_response();
                        }
                        if value.get("method").and_then(serde_json::Value::as_str)
                            == Some("custom/slow")
                        {
                            slow_started.notify_one();
                            release_slow.notified().await;
                        } else if let Some(entries) = value.as_array() {
                            if entries.iter().all(|entry| entry.get("method").is_none()) {
                                response_batch_seen.notify_one();
                            } else {
                                call_batch_seen.notify_one();
                            }
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
            .try_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
            .unwrap();
        timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap();

        caller
            .tx
            .try_send(single_frame(
                RawJsonRpcMessage::notification("custom/slow".to_string(), json!({})).unwrap(),
            ))
            .unwrap();
        timeout(Duration::from_secs(1), slow_started.notified())
            .await
            .unwrap();

        caller
            .tx
            .try_send(TransportFrame::Batch(
                TransportBatch::from_messages([
                    RawJsonRpcMessage::notification("custom/one".to_string(), json!({})).unwrap(),
                    RawJsonRpcMessage::notification("custom/two".to_string(), json!({})).unwrap(),
                ])
                .unwrap(),
            ))
            .unwrap();
        assert!(
            timeout(Duration::from_millis(100), call_batch_seen.notified())
                .await
                .is_err(),
            "call-bearing batches must remain behind an earlier ordered POST"
        );

        caller
            .tx
            .try_send(TransportFrame::Batch(
                TransportBatch::from_messages([
                    RawJsonRpcMessage::response(RequestId::Number(10), Ok(json!({}))),
                    RawJsonRpcMessage::response(RequestId::Number(11), Ok(json!({}))),
                ])
                .unwrap(),
            ))
            .unwrap();
        timeout(Duration::from_secs(1), response_batch_seen.notified())
            .await
            .expect("response-only batch should bypass the ordered POST queue");

        release_slow.notify_one();
        timeout(Duration::from_secs(1), call_batch_seen.notified())
            .await
            .expect("call-bearing batch should be sent after the earlier POST completes");

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
                .try_send(single_frame(
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
    async fn client_completion_cancels_pending_sse_establishment() {
        let sse_started = Arc::new(Notify::new());
        let delete_count = Arc::new(AtomicUsize::new(0));
        let client_finished = Arc::new(Notify::new());
        let app = Router::new().route(
            "/acp",
            post(initialize_response)
                .get({
                    let sse_started = sse_started.clone();
                    move || {
                        let sse_started = sse_started.clone();
                        async move {
                            sse_started.notify_one();
                            futures::future::pending::<StatusCode>().await
                        }
                    }
                })
                .delete({
                    let delete_count = delete_count.clone();
                    move || {
                        let delete_count = delete_count.clone();
                        async move {
                            delete_count.fetch_add(1, Ordering::SeqCst);
                            StatusCode::ACCEPTED
                        }
                    }
                }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = HttpClient::new(format!("http://{addr}")).unwrap();
        let connection = tokio::spawn(client.connect_to(InitializeThenExitClient {
            sse_started,
            finished: client_finished.clone(),
        }));

        timeout(Duration::from_secs(1), client_finished.notified())
            .await
            .expect("client foreground did not finish after the SSE request started");

        timeout(Duration::from_secs(1), connection)
            .await
            .expect("transport remained blocked on SSE response headers")
            .unwrap()
            .unwrap();
        assert_eq!(delete_count.load(Ordering::SeqCst), 1);

        server.abort();
    }

    #[tokio::test]
    async fn stalled_sse_establishment_observes_earlier_post_failure() {
        let app = Router::new().route(
            "/acp",
            get(|| async { futures::future::pending::<StatusCode>().await }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let connection = HttpConnection::new(
            url::Url::parse(&format!("http://{addr}/acp")).unwrap(),
            reqwest::Client::new(),
        );
        connection.set_connection_id("connection-1".to_string());
        let (incoming, _incoming_rx) = Channel::duplex();
        let mut state = ClientState {
            connection: connection.clone(),
            open_session_streams: HashSet::new(),
            pending_requests: HashMap::new(),
            pending_request_leases: HashMap::new(),
            incoming: incoming.tx,
        };
        let pending_request = (RequestId::Number(7), "custom/earlier".to_string());
        state.track_pending_requests(std::slice::from_ref(&pending_request));
        let mut posts = PostQueues::default();
        posts.ordered.push(PendingPost {
            pending_requests: vec![pending_request],
            cancelled_requests: Vec::new(),
            response: async { Err("earlier post failed".to_string()) }.boxed(),
        });

        let (outgoing_channel, _outgoing_peer) = Channel::duplex();
        let mut outgoing = outgoing_channel.rx;
        let mut buffered_outgoing = VecDeque::new();
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let admission = state.incoming.admission();
        let max_tasks = admission.limits().max_queued_frames;
        let mut lifecycle = HttpTransportLifecycle::new(connection, admission, max_tasks);
        let error = timeout(
            Duration::from_secs(1),
            lifecycle.start_sse(
                Some("later-session".to_string()),
                event_tx,
                SseStartContext {
                    events: &mut event_rx,
                    outgoing: &mut outgoing,
                    buffered_outgoing: &mut buffered_outgoing,
                    posts: &mut posts,
                    state: &mut state,
                },
            ),
        )
        .await
        .expect("stalled SSE setup hid an earlier POST failure")
        .unwrap_err();

        assert!(error.to_string().contains("earlier post failed"));
        assert!(state.pending_requests.is_empty());

        lifecycle.close().await;
        server.abort();
    }

    #[tokio::test]
    async fn stalled_sse_establishment_keeps_callback_responses_moving() {
        let release_get = Arc::new(Notify::new());
        let complete_earlier_post = Arc::new(Notify::new());
        let app = Router::new().route(
            "/acp",
            post({
                let release_get = release_get.clone();
                let complete_earlier_post = complete_earlier_post.clone();
                move || {
                    release_get.notify_one();
                    complete_earlier_post.notify_one();
                    async { StatusCode::ACCEPTED }
                }
            })
            .get({
                let release_get = release_get.clone();
                move || {
                    let release_get = release_get.clone();
                    async move {
                        release_get.notified().await;
                        Sse::new(futures::stream::pending::<Result<Event, Infallible>>())
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let connection = HttpConnection::new(
            url::Url::parse(&format!("http://{addr}/acp")).unwrap(),
            reqwest::Client::new(),
        );
        connection.set_connection_id("connection-1".to_string());
        let (incoming, mut incoming_peer) = Channel::duplex();
        let mut state = ClientState {
            connection: connection.clone(),
            open_session_streams: HashSet::new(),
            pending_requests: HashMap::new(),
            pending_request_leases: HashMap::new(),
            incoming: incoming.tx,
        };
        let mut posts = PostQueues::default();
        posts.ordered.push(PendingPost {
            pending_requests: Vec::new(),
            cancelled_requests: Vec::new(),
            response: async move {
                complete_earlier_post.notified().await;
                Ok(())
            }
            .boxed(),
        });

        let (outgoing_channel, outgoing_peer) = Channel::duplex();
        let outgoing_tx = outgoing_peer.tx;
        let mut outgoing = outgoing_channel.rx;
        let outgoing_guard = outgoing_tx.clone();
        let mut buffered_outgoing = VecDeque::new();
        let (mut event_tx, mut event_rx) = mpsc::channel(16);
        event_tx
            .try_send(SseMessage {
                frame: state
                    .incoming
                    .admission()
                    .try_admit(single_frame(
                        RawJsonRpcMessage::request(
                            "test/callback".to_string(),
                            json!({}),
                            RequestId::Number(99),
                        )
                        .unwrap(),
                    ))
                    .unwrap(),
            })
            .unwrap();

        let responder = async move {
            let callback = incoming_peer
                .rx
                .next()
                .await
                .expect("callback was not delivered");
            assert!(matches!(
                into_single_message(callback.into_frame()).unwrap(),
                RawJsonRpcMessage::Request(request)
                    if request.method.as_ref() == "test/callback"
            ));
            outgoing_tx
                .try_send(single_frame(RawJsonRpcMessage::response(
                    RequestId::Number(99),
                    Ok(json!({})),
                )))
                .unwrap();
        };
        let admission = state.incoming.admission();
        let max_tasks = admission.limits().max_queued_frames;
        let mut lifecycle = HttpTransportLifecycle::new(connection, admission, max_tasks);
        let (outcome, ()) = timeout(Duration::from_secs(1), async {
            futures::join!(
                lifecycle.start_sse(
                    Some("later-session".to_string()),
                    event_tx,
                    SseStartContext {
                        events: &mut event_rx,
                        outgoing: &mut outgoing,
                        buffered_outgoing: &mut buffered_outgoing,
                        posts: &mut posts,
                        state: &mut state,
                    },
                ),
                responder,
            )
        })
        .await
        .expect("callback response deadlocked behind stalled SSE establishment");

        assert_eq!(outcome.unwrap(), SseStartOutcome::Established);
        assert!(buffered_outgoing.is_empty());

        drop(outgoing_guard);
        lifecycle.close().await;
        server.abort();
    }

    #[tokio::test]
    async fn pending_sse_establishment_reports_buffered_output_on_shutdown() {
        let sse_started = Arc::new(Notify::new());
        let delete_count = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/acp",
            post(initialize_response)
                .get({
                    let sse_started = sse_started.clone();
                    move || {
                        let sse_started = sse_started.clone();
                        async move {
                            sse_started.notify_one();
                            futures::future::pending::<StatusCode>().await
                        }
                    }
                })
                .delete({
                    let delete_count = delete_count.clone();
                    move || {
                        let delete_count = delete_count.clone();
                        async move {
                            delete_count.fetch_add(1, Ordering::SeqCst);
                            StatusCode::ACCEPTED
                        }
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
            .try_send(single_frame(
                RawJsonRpcMessage::request(
                    "initialize".to_string(),
                    json!({}),
                    RequestId::Number(1),
                )
                .unwrap(),
            ))
            .unwrap();
        timeout(Duration::from_secs(1), caller.rx.next())
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(1), sse_started.notified())
            .await
            .expect("connection SSE request did not reach the server");

        caller
            .tx
            .try_send(single_frame(
                RawJsonRpcMessage::notification("custom/queued".to_string(), json!({})).unwrap(),
            ))
            .unwrap();
        drop(caller);

        let error = timeout(Duration::from_secs(1), transport)
            .await
            .expect("transport remained blocked on SSE response headers")
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("accepted messages"));
        assert_eq!(delete_count.load(Ordering::SeqCst), 1);

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
            .try_send(single_frame(
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
            .try_send(single_frame(
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
            .try_send(single_frame(RawJsonRpcMessage::response(
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
            .try_send(single_frame(
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
            .try_send(single_frame(
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
            .try_send(single_frame(
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
            .try_send(single_frame(
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

        let TransportFrame::Malformed { raw, error } = frame.frame() else {
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
        let TransportFrame::Malformed { raw, error } = frame.frame() else {
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
            .try_send(TransportFrame::Batch(
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
            .try_send(single_frame(
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
            .try_send(single_frame(
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
            .try_send(single_frame(
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
            .try_send(single_frame(
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
            .try_send(single_frame(
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
