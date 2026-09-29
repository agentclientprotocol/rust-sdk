//! Opt-in HTTP-only server. This deliberately never uses the legacy connection pumps.
use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use agent_client_protocol::{
    BoundedChannel, BoundedSender, ChannelLimits, ChargedFrame, Client, ConnectTo,
    RawJsonRpcMessage, TransportBatchEntry, TransportFrame, schema::v1::RequestId,
};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use futures::{FutureExt, StreamExt, future::BoxFuture};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot};

use super::ServerOptions;
use crate::{
    connection::ResponseRoute,
    http_server::{
        collect_route, initial_initialize_request, initialize_response_failed,
        prepare_message_route,
    },
    protocol::{HEADER_CONNECTION_ID, HEADER_SESSION_ID, JSON_MIME_TYPE, session_id_from_message},
};

/// Finite limits for the opt-in HTTP server, independent of the core channel limits.
///
/// Byte limits count encoded JSON/SSE bytes, not allocator overhead. POST bodies reserve
/// `max_frame_bytes` before their first poll. Parsed JSON and serialization scratch are
/// additional bounded multiples of that size. Each connection has its own egress budget;
/// total server egress is therefore at most `max_connections * max_egress_bytes`.
#[derive(Clone, Debug)]
pub struct ServerLimits {
    /// Core producer/transport admission limits supplied to the component factory.
    pub channel_limits: ChannelLimits,
    pub max_connections: usize,
    pub max_in_flight_posts: usize,
    pub max_body_bytes: usize,
    pub max_frame_bytes: usize,
    pub max_batch_entries: usize,
    pub max_pending_routes: usize,
    pub max_registered_sessions: usize,
    pub max_active_streams: usize,
    pub max_egress_frames: usize,
    pub max_egress_bytes: usize,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            channel_limits: ChannelLimits {
                max_frame_bytes: 256 * 1024,
                ..ChannelLimits::default()
            },
            max_connections: 64,
            max_in_flight_posts: 32,
            max_body_bytes: 8 * 1024 * 1024,
            max_frame_bytes: 256 * 1024,
            max_batch_entries: 128,
            max_pending_routes: 256,
            max_registered_sessions: 64,
            max_active_streams: 65,
            max_egress_frames: 64,
            max_egress_bytes: 4 * 1024 * 1024,
        }
    }
}

/// An invalid bounded server configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerLimitsError(pub &'static str);

impl std::fmt::Display for ServerLimitsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ServerLimitsError {}

impl ServerLimits {
    pub fn validate(&self) -> Result<(), ServerLimitsError> {
        self.channel_limits
            .validate()
            .map_err(|_| ServerLimitsError("invalid core channel limits"))?;
        for value in [
            self.max_connections,
            self.max_in_flight_posts,
            self.max_body_bytes,
            self.max_frame_bytes,
            self.max_batch_entries,
            self.max_pending_routes,
            self.max_registered_sessions,
            self.max_active_streams,
            self.max_egress_frames,
            self.max_egress_bytes,
        ] {
            if value == 0 || value > u32::MAX as usize || value > Semaphore::MAX_PERMITS {
                return Err(ServerLimitsError(
                    "limits must be nonzero and fit semaphore counters",
                ));
            }
        }
        if self.max_body_bytes < self.max_frame_bytes
            || self.max_egress_bytes < self.max_frame_bytes + 8
        {
            return Err(ServerLimitsError(
                "body and egress budgets must fit one maximum frame",
            ));
        }
        if self
            .max_connections
            .checked_mul(self.max_egress_bytes)
            .is_none()
        {
            return Err(ServerLimitsError("aggregate egress budget overflows usize"));
        }
        Ok(())
    }
}

type Factory = dyn Fn(
        ChannelLimits,
    ) -> agent_client_protocol::Result<(
        BoundedChannel,
        BoxFuture<'static, agent_client_protocol::Result<()>>,
    )> + Send
    + Sync;

/// HTTP/SSE-only bounded server. WebSocket upgrades are explicitly rejected.
///
/// Components are connected via `ConnectTo::into_bounded_channel_and_future`. Components
/// that only extract legacy channels fail closed; no unbounded adapter is introduced.
/// Admission errors before core enqueue return 429/413. An admitted frame is never retried;
/// later overload terminates the connection. Body polling is not peer acknowledgment.
/// POST admission is fail-fast. Saturation of POST/body slots also terminates the
/// addressed connection, so callback responses cannot remain indefinitely blocked behind
/// slow request bodies. Callers must not automatically retry uncertain POST results.
/// Registered session mailboxes persist until connection termination; replay is not provided.
pub struct BoundedAcpHttpServer {
    state: Arc<Registry>,
    options: ServerOptions,
}

impl std::fmt::Debug for BoundedAcpHttpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedAcpHttpServer")
            .field("limits", &self.state.limits)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl BoundedAcpHttpServer {
    pub(super) fn new<F, C>(factory: F, limits: ServerLimits) -> Result<Self, ServerLimitsError>
    where
        F: Fn() -> C + Send + Sync + 'static,
        C: ConnectTo<Client>,
    {
        limits.validate()?;
        Ok(Self {
            state: Arc::new(Registry {
                factory: Arc::new(move |limits| factory().into_bounded_channel_and_future(limits)),
                connections: Mutex::new(HashMap::new()),
                connection_slots: Arc::new(Semaphore::new(limits.max_connections)),
                posts: Arc::new(Semaphore::new(limits.max_in_flight_posts)),
                bodies: Arc::new(Semaphore::new(limits.max_body_bytes)),
                limits,
                graceful_delete: None,
            }),
            options: ServerOptions::default(),
        })
    }

    #[must_use]
    pub fn with_options(mut self, options: ServerOptions) -> Self {
        self.options = options;
        self
    }

    /// Opt into graceful DELETE with a bounded wait per HTTP request.
    ///
    /// DELETE seals inbound admission without allocating a body or core frame. It
    /// returns 202 only after the component future succeeds and its output reaches
    /// clean EOF. This is not acknowledgment that an HTTP peer consumed the output.
    /// A deadline or terminal failure returns 503. A deadline never reopens admission
    /// or aborts accepted work; canceling the DELETE request only drops its waiter.
    /// Concurrent/repeated DELETE requests join the connection-owned drain while it
    /// exists. Once completion removes the connection, subsequent requests return 404.
    /// POST requests racing with the seal are rejected with 410 before enqueue.
    /// Without this option, DELETE retains its legacy abortive behavior.
    #[must_use]
    pub fn with_graceful_delete(mut self, timeout: Duration) -> Self {
        // The server has not exposed the registry before into_router consumes it.
        Arc::get_mut(&mut self.state).unwrap().graceful_delete = Some(timeout);
        self
    }

    pub fn into_router(self) -> Router {
        let mut router = Router::new()
            .route(&self.options.path, post(handle_post))
            .route(&self.options.path, get(handle_get))
            .route(&self.options.path, delete(handle_delete))
            .with_state(self.state);
        if self.options.health_endpoint {
            router = router.route("/health", get(super::health));
        }
        if let Some(origin) = self.options.cors.allow_origin_layer() {
            router = router.layer(super::default_cors(origin));
        }
        router
    }
}

struct Registry {
    graceful_delete: Option<Duration>,
    factory: Arc<Factory>,
    limits: ServerLimits,
    connections: Mutex<HashMap<String, Arc<Connection>>>,
    connection_slots: Arc<Semaphore>,
    posts: Arc<Semaphore>,
    bodies: Arc<Semaphore>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        for connection in self.connections.get_mut().unwrap().values() {
            connection.close();
        }
    }
}

struct Connection {
    id: String,
    registry: Weak<Registry>,
    limits: ServerLimits,
    inner: Mutex<ConnectionState>,
    wake: Notify,
    frame_slots: Arc<Semaphore>,
    byte_slots: Arc<Semaphore>,
    stream_slots: Arc<Semaphore>,
    // Held until tasks, requests and response bodies have released this connection.
    _slot: OwnedSemaphorePermit,
}

struct ConnectionState {
    tx: Option<BoundedSender>,
    closed: bool,
    draining: bool,
    drain_result: Option<bool>,
    task: Option<tokio::task::AbortHandle>,
    pending: VecDeque<(RequestId, ResponseRoute)>,
    streams: HashMap<Option<String>, Mailbox>,
}

#[derive(Default)]
struct Mailbox {
    queue: VecDeque<Envelope>,
    subscribed: bool,
}

struct Envelope {
    // Keep the core byte/frame admission across decode, routing and HTTP body handoff.
    frame: ChargedFrame,
    _frame_slot: OwnedSemaphorePermit,
    _byte_slot: OwnedSemaphorePermit,
}

impl Connection {
    fn close(&self) {
        self.close_if_open(false);
    }

    // A POST that raced with DELETE must not turn a harmless admission rejection
    // into terminal core failure. All destructors and wakeups run after unlocking.
    fn close_if_open(&self, only_open: bool) -> bool {
        let retired = {
            let mut state = self.inner.lock().unwrap();
            if only_open && (state.draining || state.closed) {
                return false;
            }
            state.closed = true;
            state.drain_result.get_or_insert(false);
            (
                state.tx.take(),
                state.task.take(),
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.streams),
            )
        };
        if let Some(tx) = &retired.0 {
            tx.fail("HTTP connection terminated");
        }
        if let Some(task) = &retired.1 {
            task.abort();
        }
        drop(retired);
        self.wake.notify_waiters();
        true
    }

    fn remove(&self) {
        if let Some(registry) = self.registry.upgrade() {
            let removed = registry.connections.lock().unwrap().remove(&self.id);
            drop(removed);
        }
    }

    fn terminate(&self) {
        self.close();
        self.remove();
    }

    fn terminate_if_open(&self) {
        if self.close_if_open(true) {
            self.remove();
        }
    }

    fn begin_drain(&self) {
        let tx = {
            let mut state = self.inner.lock().unwrap();
            if state.closed || state.draining {
                return;
            }
            // This is the HTTP acceptance linearization point, shared with enqueue.
            state.draining = true;
            state.tx.take()
        };
        // No await separates seal publication and the core close. Cancellation cannot
        // strand this operation; close preserves all already accepted frame charges.
        if let Some(tx) = tx {
            tx.close_channel();
        }
    }

    fn finish_drain(&self) -> bool {
        let finished = {
            let mut state = self.inner.lock().unwrap();
            if state.closed || !state.draining {
                return false;
            }
            state.closed = true;
            state.drain_result = Some(true);
            state.task.take()
        };
        drop(finished);
        self.wake.notify_waiters();
        self.remove();
        true
    }

    async fn drain_result(&self) -> bool {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.inner.lock().unwrap().drain_result {
                return result;
            }
            notified.await;
        }
    }

    fn envelope(&self, frame: ChargedFrame) -> Result<Envelope, ()> {
        let len = frame.as_bytes().len();
        if len > self.limits.max_frame_bytes || frame.as_bytes().contains(&b'\r') {
            return Err(());
        }
        // Reserve actual SSE framing too, before its allocation. JSON is UTF-8; the
        // serializer normally emits one line, but raw/malformed frames may contain LF.
        let text = std::str::from_utf8(frame.as_bytes()).map_err(|_| ())?;
        let wire_len = len
            .checked_add(8)
            .and_then(|n| n.checked_add(text.matches('\n').count() * 6))
            .ok_or(())?;
        let frames = self
            .frame_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ())?;
        let bytes = self
            .byte_slots
            .clone()
            .try_acquire_many_owned(u32::try_from(wire_len).map_err(|_| ())?)
            .map_err(|_| ())?;
        Ok(Envelope {
            frame,
            _frame_slot: frames,
            _byte_slot: bytes,
        })
    }

    fn complete_initial_routes(&self, frame: &TransportFrame) {
        let mut state = self.inner.lock().unwrap();
        let mut complete = |message: &RawJsonRpcMessage| {
            if message.response_id().is_some() {
                outbound_route(&mut state.pending, message);
            }
        };
        match frame {
            TransportFrame::Single(message) => complete(message),
            TransportFrame::Batch(batch) => {
                for entry in batch.entries() {
                    if let TransportBatchEntry::Message(message) = entry {
                        complete(message);
                    }
                }
            }
            TransportFrame::Malformed { .. } => {}
        }
    }

    fn route(&self, envelope: Envelope, frame: &TransportFrame) -> Result<(), ()> {
        let mut state = self.inner.lock().unwrap();
        if state.closed {
            return Err(());
        }
        let route = match frame {
            TransportFrame::Single(message) => outbound_route(&mut state.pending, message),
            TransportFrame::Malformed { .. } => ResponseRoute::Connection,
            TransportFrame::Batch(batch) => {
                let mut common = None;
                let mut mixed = false;
                for entry in batch.entries() {
                    let route = match entry {
                        TransportBatchEntry::Message(message) => {
                            outbound_route(&mut state.pending, message)
                        }
                        TransportBatchEntry::Malformed { .. } => ResponseRoute::Connection,
                    };
                    match &common {
                        None => common = Some(route),
                        Some(previous) if previous == &route => {}
                        Some(_) => mixed = true,
                    }
                }
                if mixed {
                    ResponseRoute::Connection
                } else {
                    common.unwrap_or(ResponseRoute::Connection)
                }
            }
        };
        let key = route_key(route);
        ensure_stream(&mut state, key, self.limits.max_registered_sessions)?
            .queue
            .push_back(envelope);
        drop(state);
        self.wake.notify_waiters();
        Ok(())
    }
}

fn route_key(route: ResponseRoute) -> Option<String> {
    match route {
        ResponseRoute::Connection => None,
        ResponseRoute::Session(id) => Some(id),
    }
}

fn ensure_stream(
    state: &mut ConnectionState,
    key: Option<String>,
    max: usize,
) -> Result<&mut Mailbox, ()> {
    if !state.streams.contains_key(&key) && key.is_some() && state.streams.len() > max {
        return Err(());
    }
    Ok(state.streams.entry(key).or_default())
}

fn outbound_route(
    pending: &mut VecDeque<(RequestId, ResponseRoute)>,
    message: &RawJsonRpcMessage,
) -> ResponseRoute {
    if let Some(id) = message.response_id() {
        if let Some(index) = pending.iter().position(|(pending_id, _)| pending_id == id) {
            return pending.remove(index).unwrap().1;
        }
        return ResponseRoute::Connection;
    }
    session_id_from_message(message).map_or(ResponseRoute::Connection, ResponseRoute::Session)
}

struct Cleanup {
    connection: Arc<Connection>,
    armed: bool,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.armed {
            self.connection.terminate();
        }
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Result<Option<String>, StatusCode> {
    headers
        .get(name)
        .map(|value| {
            if value.len() > 1024 {
                return Err(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
            }
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| StatusCode::BAD_REQUEST)
        })
        .transpose()
}

fn check_batch(frame: &TransportFrame, max: usize) -> bool {
    match frame {
        TransportFrame::Batch(batch) => batch.entries().take(max + 1).count() <= max,
        _ => true,
    }
}

// Serialize into a capped writer before core enqueue, including session-header injection.
fn encode_frame(frame: &TransportFrame, max: usize) -> Result<String, StatusCode> {
    struct Capped {
        bytes: Vec<u8>,
        max: usize,
    }
    impl std::io::Write for Capped {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.max - self.bytes.len() {
                return Err(std::io::Error::other("frame limit exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Capped {
        bytes: Vec::new(),
        max,
    };
    let result = match frame {
        TransportFrame::Single(message) => serde_json::to_writer(&mut output, message),
        TransportFrame::Batch(batch) => serde_json::to_writer(&mut output, batch),
        TransportFrame::Malformed { raw, .. } => {
            if raw.len() > max {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            return Ok(raw.clone());
        }
    };
    result.map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    Ok(String::from_utf8(output.bytes).expect("JSON is UTF-8"))
}

async fn handle_post(State(registry): State<Arc<Registry>>, request: Request<Body>) -> Response {
    match post_inner(registry, request).await {
        Ok(response) => response,
        Err(status) => status.into_response(),
    }
}

async fn post_inner(
    registry: Arc<Registry>,
    request: Request<Body>,
) -> Result<Response, StatusCode> {
    if !request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with(JSON_MIME_TYPE))
    {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let connection_id = header_value(request.headers(), HEADER_CONNECTION_ID)?;
    let session_id = header_value(request.headers(), HEADER_SESSION_ID)?;
    // Reject closing connections before reserving global POST/body capacity. A
    // second check at enqueue handles bodies already being read when DELETE seals.
    let existing = if let Some(id) = &connection_id {
        let connection = registry.connections.lock().unwrap().get(id).cloned();
        if let Some(connection) = &connection {
            let state = connection.inner.lock().unwrap();
            if state.draining || state.closed {
                return Err(StatusCode::GONE);
            }
        }
        connection
    } else {
        None
    };

    // Body shape is unknown until read: rather than let slow request bodies starve
    // callback responses indefinitely, saturation terminates the addressed connection.
    // The rejected POST itself has not been accepted and is never resubmitted here.
    let saturated = || {
        if let Some(id) = &connection_id {
            let connection = registry.connections.lock().unwrap().get(id).cloned();
            if let Some(connection) = connection {
                connection.terminate_if_open();
            }
        }
        StatusCode::TOO_MANY_REQUESTS
    };
    let post_slot = registry
        .posts
        .clone()
        .try_acquire_owned()
        .map_err(|_| saturated())?;
    let body_slot = registry
        .bodies
        .clone()
        .try_acquire_many_owned(
            u32::try_from(registry.limits.max_frame_bytes).expect("validated frame limit"),
        )
        .map_err(|_| saturated())?;
    // Admission before the first body read and before factory invocation.
    let creating = connection_id.is_none();
    let connection_slot = if creating {
        Some(
            registry
                .connection_slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?,
        )
    } else {
        None
    };
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|n| n > registry.limits.max_frame_bytes)
    {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let body = axum::body::to_bytes(request.into_body(), registry.limits.max_frame_bytes)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    let text = std::str::from_utf8(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let mut frame = TransportFrame::parse_json(text);
    if !check_batch(&frame, registry.limits.max_batch_entries) {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let initialize = initial_initialize_request(&frame).map(|(id, _)| id.clone());
    if creating != initialize.is_some() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut sessions = Vec::new();
    let mut pending = Vec::new();
    let mut prepare = |message: &mut RawJsonRpcMessage| -> Result<(), StatusCode> {
        let route = prepare_message_route(message, session_id.as_deref())
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        collect_route(message, route, &mut sessions, &mut pending);
        Ok(())
    };
    match &mut frame {
        TransportFrame::Single(message) => prepare(message)?,
        TransportFrame::Batch(batch) => {
            for entry in batch.entries_mut() {
                if let TransportBatchEntry::Message(message) = entry {
                    prepare(message)?;
                }
            }
        }
        TransportFrame::Malformed { .. } => {}
    }
    // Reject impossible route reservations before any factory work. Existing connection
    // totals are checked transactionally below, immediately before acceptance.
    if pending.len() > registry.limits.max_pending_routes {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    sessions.sort();
    sessions.dedup();
    if sessions.len() > registry.limits.max_registered_sessions {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    let encoded = encode_frame(&frame, registry.limits.max_frame_bytes)?;
    drop(frame);
    let mut initialization = None;
    let connection = if connection_id.is_some() {
        existing.ok_or(StatusCode::NOT_FOUND)?
    } else {
        let (BoundedChannel { tx, mut rx }, agent) =
            (registry.factory)(registry.limits.channel_limits)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let terminal_owner = tx.clone();
        let failure = tx.failure();
        let graceful = registry.graceful_delete.is_some();
        let mut streams = HashMap::new();
        streams.insert(None, Mailbox::default());
        let connection = Arc::new(Connection {
            id: uuid::Uuid::new_v4().to_string(),
            registry: Arc::downgrade(&registry),
            limits: registry.limits.clone(),
            inner: Mutex::new(ConnectionState {
                tx: Some(tx),
                closed: false,
                draining: false,
                drain_result: None,
                task: None,
                pending: VecDeque::new(),
                streams,
            }),
            wake: Notify::new(),
            frame_slots: Arc::new(Semaphore::new(registry.limits.max_egress_frames)),
            byte_slots: Arc::new(Semaphore::new(registry.limits.max_egress_bytes)),
            stream_slots: Arc::new(Semaphore::new(registry.limits.max_active_streams)),
            _slot: connection_slot.unwrap(),
        });
        let (init_tx, init_rx) = oneshot::channel();
        initialization = Some(init_rx);
        registry
            .connections
            .lock()
            .unwrap()
            .insert(connection.id.clone(), connection.clone());
        let cleanup = Cleanup {
            connection: connection.clone(),
            armed: true,
        };
        let init_id = initialize.unwrap();
        let task_connection = connection.clone();
        // Capture cleanup before spawning: dropping even a never-polled task cleans up.
        let task = tokio::spawn(async move {
            let mut cleanup = cleanup;
            // Keep the terminal signal owner alive: dropping every channel endpoint
            // cancels its signal, which is not evidence of a core failure.
            let _terminal_owner = terminal_owner;
            let completion_connection = task_connection.clone();
            let router = async move {
                let mut init_tx = Some(init_tx);
                while let Some(charged) = rx.next().await {
                    if charged.as_bytes().len() > task_connection.limits.max_frame_bytes {
                        return Err(());
                    }
                    let frame = charged.decode();
                    if !check_batch(&frame, task_connection.limits.max_batch_entries) {
                        return Err(());
                    }
                    let Ok(envelope) = task_connection.envelope(charged) else {
                        return Err(());
                    };
                    if let Some(failed) = initialize_response_failed(&frame, &init_id)
                        && let Some(sender) = init_tx.take()
                    {
                        task_connection.complete_initial_routes(&frame);
                        if sender.send((envelope, failed)).is_err() {
                            return Err(());
                        }
                        continue;
                    }
                    if task_connection.route(envelope, &frame).is_err() {
                        return Err(());
                    }
                }
                Ok::<(), ()>(())
            };
            if graceful {
                // EOF alone is insufficient: core failure also produces EOF. Poll
                // failure first and again after both real futures have completed.
                let done = async {
                    futures::try_join!(router, async { agent.await.map_err(|_| ()) }).map(|_| ())
                };
                tokio::pin!(failure);
                let result = tokio::select! {
                    biased;
                    _ = &mut failure => Err(()),
                    result = done => result,
                };
                if result.is_ok()
                    && failure.now_or_never().is_none()
                    && completion_connection.finish_drain()
                {
                    cleanup.armed = false;
                }
            } else {
                // A bare channel's driver is immediately successful. Legacy mode
                // keeps routing until EOF instead of dropping escaped producers.
                let agent = async move {
                    if agent.await.is_ok() {
                        futures::future::pending::<()>().await;
                    }
                };
                futures::pin_mut!(agent, router);
                let _ = futures::future::select(router, agent).await;
            }
        });
        let abort = {
            let mut state = connection.inner.lock().unwrap();
            if state.closed {
                true
            } else {
                state.task = Some(task.abort_handle());
                false
            }
        };
        if abort {
            task.abort();
        }
        connection
    };
    let mut cleanup = Cleanup {
        connection: connection.clone(),
        armed: initialization.is_some(),
    };
    {
        // One lock makes route reservation, duplicate-ID ordering and enqueue transactional.
        // There is no await/cancellation point between bookkeeping and core acceptance.
        let mut state = connection.inner.lock().unwrap();
        if state.closed || state.draining {
            return Err(StatusCode::GONE);
        }
        if state
            .pending
            .len()
            .checked_add(pending.len())
            .is_none_or(|n| n > connection.limits.max_pending_routes)
        {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        sessions.sort();
        sessions.dedup();
        let new_sessions = sessions
            .iter()
            .filter(|id| !state.streams.contains_key(&Some((*id).clone())))
            .count();
        if state.streams.len() - 1 + new_sessions > connection.limits.max_registered_sessions {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        // try_send is the acceptance boundary. A core error can close its bounded channel;
        // terminate instead of implying that resubmission of this POST is safe.
        if state
            .tx
            .as_ref()
            .unwrap()
            .try_send_serialized(&encoded)
            .is_err()
        {
            drop(state);
            connection.terminate();
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
        for id in sessions {
            state.streams.entry(Some(id)).or_default();
        }
        state.pending.extend(pending);
    }
    drop(encoded);
    drop(body);
    drop(body_slot);
    drop(post_slot);
    let Some(initialization) = initialization else {
        return Ok(StatusCode::ACCEPTED.into_response());
    };
    let (envelope, failed) = initialization
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if failed {
        connection.terminate();
    } else if connection.inner.lock().unwrap().closed {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    // An unpolled/dropped initialization body still owns cleanup. Only yielding its bytes
    // publishes success; this is an HTTP-stack handoff, not a peer acknowledgment.
    let id = connection.id.clone();
    let stream = async_stream::stream! {
        let envelope = envelope;
        let bytes = envelope.frame.as_bytes().to_vec();
        cleanup.armed = false;
        yield Ok::<_, Infallible>(bytes);
        drop(envelope);
        drop(cleanup);
    };
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(JSON_MIME_TYPE),
    );
    if !failed {
        response
            .headers_mut()
            .insert(HEADER_CONNECTION_ID, HeaderValue::from_str(&id).unwrap());
    }
    Ok(response)
}

struct Lease {
    connection: Arc<Connection>,
    key: Option<String>,
    _slot: OwnedSemaphorePermit,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(mailbox) = self
            .connection
            .inner
            .lock()
            .unwrap()
            .streams
            .get_mut(&self.key)
        {
            mailbox.subscribed = false;
        }
    }
}

async fn handle_get(State(registry): State<Arc<Registry>>, request: Request<Body>) -> Response {
    match get_inner(registry, request) {
        Ok(response) => response,
        Err(status) => status.into_response(),
    }
}

fn get_inner(registry: Arc<Registry>, request: Request<Body>) -> Result<Response, StatusCode> {
    if request.headers().contains_key(header::UPGRADE) {
        return Err(StatusCode::NOT_IMPLEMENTED);
    }
    let id =
        header_value(request.headers(), HEADER_CONNECTION_ID)?.ok_or(StatusCode::BAD_REQUEST)?;
    let key = header_value(request.headers(), HEADER_SESSION_ID)?;
    let connection = registry
        .connections
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;
    let slot = connection
        .stream_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    {
        let mut state = connection.inner.lock().unwrap();
        if state.closed {
            return Err(StatusCode::GONE);
        }
        let mailbox = ensure_stream(
            &mut state,
            key.clone(),
            connection.limits.max_registered_sessions,
        )
        .map_err(|()| StatusCode::TOO_MANY_REQUESTS)?;
        if mailbox.subscribed {
            return Err(StatusCode::CONFLICT);
        }
        mailbox.subscribed = true;
    }
    let lease = Lease {
        connection,
        key: key.clone(),
        _slot: slot,
    };
    let stream = async_stream::stream! {
        let lease = lease;
        loop {
            // Register before checking the queue to avoid a lost notify_waiters race.
            let notified = lease.connection.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (envelope, closed) = {
                let mut state = lease.connection.inner.lock().unwrap();
                let envelope = state.streams.get_mut(&lease.key).and_then(|m| m.queue.pop_front());
                (envelope, state.closed)
            };
            if let Some(envelope) = envelope {
                let text = std::str::from_utf8(envelope.frame.as_bytes()).expect("serialized JSON is UTF-8");
                let mut event = String::with_capacity(text.len() + 8 + text.matches('\n').count() * 6);
                for line in text.split('\n') { event.push_str("data: "); event.push_str(line); event.push('\n'); }
                event.push('\n');
                yield Ok::<_, Infallible>(event.into_bytes());
                // Retained through yield; release on next body poll or cancellation/drop.
                drop(envelope);
            } else if closed { break; } else { notified.await; }
        }
    };
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert(HEADER_CONNECTION_ID, HeaderValue::from_str(&id).unwrap());
    if let Some(key) = key {
        response
            .headers_mut()
            .insert(HEADER_SESSION_ID, HeaderValue::from_str(&key).unwrap());
    }
    Ok(response)
}

async fn handle_delete(State(registry): State<Arc<Registry>>, request: Request<Body>) -> Response {
    let Ok(Some(id)) = header_value(request.headers(), HEADER_CONNECTION_ID) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let connection = registry.connections.lock().unwrap().get(&id).cloned();
    let Some(connection) = connection else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(deadline) = registry.graceful_delete {
        connection.begin_drain();
        return match tokio::time::timeout(deadline, connection.drain_result()).await {
            Ok(true) => StatusCode::ACCEPTED,
            Ok(false) | Err(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
        .into_response();
    }
    connection.terminate();
    StatusCode::ACCEPTED.into_response()
}

#[cfg(test)]
#[path = "graceful_delete_tests.rs"]
mod graceful_delete_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{Duration, timeout};

    fn registry(limits: ServerLimits) -> (Arc<Registry>, BoundedChannel) {
        let (agent, transport) = BoundedChannel::duplex(limits.channel_limits).unwrap();
        let transport = Mutex::new(Some(transport));
        let server =
            BoundedAcpHttpServer::new(move || transport.lock().unwrap().take().unwrap(), limits)
                .unwrap();
        (server.state, agent)
    }

    fn post_request(body: impl Into<Body>, id: Option<&str>) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .header(header::CONTENT_TYPE, JSON_MIME_TYPE);
        if let Some(id) = id {
            request = request.header(HEADER_CONNECTION_ID, id);
        }
        request.body(body.into()).unwrap()
    }

    async fn initialize(registry: Arc<Registry>, agent: &mut BoundedChannel) -> (String, Response) {
        let task = tokio::spawn(handle_post(
            State(registry),
            post_request(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                None,
            ),
        ));
        let received = timeout(Duration::from_secs(2), agent.rx.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(received.decode(), TransportFrame::Single(_)));
        drop(received);
        agent
            .tx
            .try_send_serialized(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
            .unwrap();
        let response = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let id = response.headers()[HEADER_CONNECTION_ID]
            .to_str()
            .unwrap()
            .to_owned();
        (id, response)
    }

    #[test]
    fn validates_finite_limits_and_capped_encoding() {
        let limits = ServerLimits {
            max_connections: 0,
            ..ServerLimits::default()
        };
        assert!(limits.validate().is_err());
        let frame = TransportFrame::parse_json(r#"{"jsonrpc":"2.0","method":"ping"}"#);
        let encoded = encode_frame(&frame, 1024).unwrap();
        assert!(encode_frame(&frame, encoded.len()).is_ok());
        assert_eq!(
            encode_frame(&frame, encoded.len() - 1),
            Err(StatusCode::PAYLOAD_TOO_LARGE)
        );
    }

    #[tokio::test]
    async fn post_and_connection_admission_precede_body_poll_and_factory() {
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = calls.clone();
        let state = BoundedAcpHttpServer::new(
            move || -> BoundedChannel {
                factory_calls.fetch_add(1, Ordering::SeqCst);
                panic!("factory must not run");
            },
            ServerLimits {
                max_connections: 1,
                max_in_flight_posts: 1,
                ..ServerLimits::default()
            },
        )
        .unwrap()
        .state;
        let polls = Arc::new(AtomicUsize::new(0));
        for hold_connection in [false, true] {
            let held = if hold_connection {
                state.connection_slots.clone()
            } else {
                state.posts.clone()
            }
            .try_acquire_owned()
            .unwrap();
            let body_polls = polls.clone();
            let body = Body::from_stream(futures::stream::poll_fn(move |_| {
                body_polls.fetch_add(1, Ordering::SeqCst);
                std::task::Poll::Ready(Some(Ok::<_, Infallible>("{}")))
            }));
            let response = handle_post(State(state.clone()), post_request(body, None)).await;
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(polls.load(Ordering::SeqCst), 0);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            drop(held);
        }
        assert_eq!(state.posts.available_permits(), 1);
        assert_eq!(
            state.bodies.available_permits(),
            state.limits.max_body_bytes
        );
    }

    #[tokio::test]
    async fn dropping_unpolled_initialize_body_terminates_connection() {
        let (state, mut agent) = registry(ServerLimits::default());
        let (id, response) = initialize(state.clone(), &mut agent).await;
        assert!(state.connections.lock().unwrap().contains_key(&id));
        drop(response);
        assert!(!state.connections.lock().unwrap().contains_key(&id));
        tokio::task::yield_now().await;
        assert_eq!(
            state.connection_slots.available_permits(),
            state.limits.max_connections
        );
    }

    #[tokio::test]
    async fn dropped_initialization_request_cleans_up_without_detached_cleanup() {
        let (state, mut agent) = registry(ServerLimits::default());
        let task = tokio::spawn(handle_post(
            State(state.clone()),
            post_request(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                None,
            ),
        ));
        let received = timeout(Duration::from_secs(2), agent.rx.next())
            .await
            .unwrap()
            .unwrap();
        drop(received);
        task.abort();
        drop(task.await);
        tokio::task::yield_now().await;
        assert!(state.connections.lock().unwrap().is_empty());
        assert_eq!(
            state.posts.available_permits(),
            state.limits.max_in_flight_posts
        );
        assert_eq!(
            state.bodies.available_permits(),
            state.limits.max_body_bytes
        );
    }

    #[tokio::test]
    async fn sse_lease_and_egress_charge_survive_body_handoff_until_drop() {
        let (state, mut agent) = registry(ServerLimits::default());
        let (id, response) = initialize(state.clone(), &mut agent).await;
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let connection = state.connections.lock().unwrap().get(&id).unwrap().clone();
        agent
            .tx
            .try_send_serialized(r#"{"jsonrpc":"2.0","method":"notice"}"#)
            .unwrap();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            connection.frame_slots.available_permits(),
            state.limits.max_egress_frames - 1
        );
        let get = || {
            Request::builder()
                .header(HEADER_CONNECTION_ID, &id)
                .body(Body::empty())
                .unwrap()
        };
        let response = get_inner(state.clone(), get()).unwrap();
        assert_eq!(
            get_inner(state.clone(), get()).unwrap_err(),
            StatusCode::CONFLICT
        );
        let mut body = response.into_body().into_data_stream();
        let bytes = timeout(Duration::from_secs(2), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(std::str::from_utf8(&bytes).unwrap().starts_with("data: "));
        assert_eq!(
            connection.frame_slots.available_permits(),
            state.limits.max_egress_frames - 1
        );
        drop(body);
        assert_eq!(
            connection.frame_slots.available_permits(),
            state.limits.max_egress_frames
        );
        assert_eq!(
            connection.byte_slots.available_permits(),
            state.limits.max_egress_bytes
        );
        assert_eq!(
            connection.stream_slots.available_permits(),
            state.limits.max_active_streams
        );
        assert!(get_inner(state.clone(), get()).is_ok());
        connection.terminate();
    }

    #[tokio::test]
    async fn batched_initialize_releases_all_response_routes() {
        let (state, mut agent) = registry(ServerLimits::default());
        let task = tokio::spawn(handle_post(
            State(state.clone()),
            post_request(
                r#"[{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}},{"jsonrpc":"2.0","id":2,"method":"ping"}]"#,
                None,
            ),
        ));
        let received = timeout(Duration::from_secs(2), agent.rx.next())
            .await
            .unwrap()
            .unwrap();
        drop(received);
        agent
            .tx
            .try_send_serialized(
                r#"[{"jsonrpc":"2.0","id":1,"result":{}},{"jsonrpc":"2.0","id":2,"result":{}}]"#,
            )
            .unwrap();
        let response = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        let id = response.headers()[HEADER_CONNECTION_ID]
            .to_str()
            .unwrap()
            .to_owned();
        let connection = state.connections.lock().unwrap().get(&id).unwrap().clone();
        assert!(connection.inner.lock().unwrap().pending.is_empty());
        drop(response);
    }

    #[tokio::test]
    async fn duplicate_route_cap_is_atomic_and_unknown_session_gets_are_bounded() {
        let (state, mut agent) = registry(ServerLimits {
            max_pending_routes: 2,
            max_registered_sessions: 1,
            ..ServerLimits::default()
        });
        let (id, response) = initialize(state.clone(), &mut agent).await;
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let request = r#"[{"jsonrpc":"2.0","id":2,"method":"ping"},{"jsonrpc":"2.0","id":2,"method":"ping"}]"#;
        let response = handle_post(State(state.clone()), post_request(request, Some(&id))).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let connection = state.connections.lock().unwrap().get(&id).unwrap().clone();
        assert_eq!(connection.inner.lock().unwrap().pending.len(), 2);
        let response = handle_post(State(state.clone()), post_request(request, Some(&id))).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(connection.inner.lock().unwrap().pending.len(), 2);
        let get = |session| {
            Request::builder()
                .header(HEADER_CONNECTION_ID, &id)
                .header(HEADER_SESSION_ID, session)
                .body(Body::empty())
                .unwrap()
        };
        drop(get_inner(state.clone(), get("one")).unwrap());
        assert_eq!(
            get_inner(state.clone(), get("two")).unwrap_err(),
            StatusCode::TOO_MANY_REQUESTS
        );
        connection.terminate();
        assert!(connection.inner.lock().unwrap().pending.is_empty());
    }

    #[tokio::test]
    async fn oversized_unknown_length_body_and_batch_restore_admission() {
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = calls.clone();
        let state = BoundedAcpHttpServer::new(
            move || -> BoundedChannel {
                factory_calls.fetch_add(1, Ordering::SeqCst);
                panic!("oversized bodies must not reach the factory");
            },
            ServerLimits {
                max_frame_bytes: 128,
                max_batch_entries: 1,
                ..ServerLimits::default()
            },
        )
        .unwrap()
        .state;
        let body = Body::from_stream(futures::stream::iter([Ok::<_, Infallible>(vec![
            b' ';
            129
        ])]));
        assert_eq!(
            handle_post(State(state.clone()), post_request(body, None))
                .await
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let batch = r#"[{"jsonrpc":"2.0","id":1,"method":"initialize"},{"jsonrpc":"2.0","id":2,"method":"ping"}]"#;
        assert_eq!(
            handle_post(State(state.clone()), post_request(batch, None))
                .await
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            state.posts.available_permits(),
            state.limits.max_in_flight_posts
        );
        assert_eq!(
            state.bodies.available_permits(),
            state.limits.max_body_bytes
        );
        assert_eq!(
            state.connection_slots.available_permits(),
            state.limits.max_connections
        );
    }

    #[tokio::test]
    async fn accepted_egress_overflow_terminates_and_revokes_escaped_sender() {
        let (state, mut agent) = registry(ServerLimits {
            max_egress_frames: 1,
            ..ServerLimits::default()
        });
        let (id, response) = initialize(state.clone(), &mut agent).await;
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let connection = state.connections.lock().unwrap().get(&id).unwrap().clone();
        for _ in 0..2 {
            agent
                .tx
                .try_send_serialized(r#"{"jsonrpc":"2.0","method":"notice"}"#)
                .unwrap();
        }
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(state.connections.lock().unwrap().is_empty());
        assert!(
            agent
                .tx
                .try_send_serialized(r#"{"jsonrpc":"2.0","method":"notice"}"#)
                .is_err()
        );
        assert_eq!(
            connection.frame_slots.available_permits(),
            state.limits.max_egress_frames
        );
        assert_eq!(
            connection.byte_slots.available_permits(),
            state.limits.max_egress_bytes
        );
        assert!(connection.inner.lock().unwrap().streams.is_empty());
    }

    #[tokio::test]
    async fn response_body_saturation_terminates_instead_of_deadlocking_callback() {
        let (state, mut agent) = registry(ServerLimits {
            max_in_flight_posts: 1,
            ..ServerLimits::default()
        });
        let (id, response) = initialize(state.clone(), &mut agent).await;
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let _held = state.posts.clone().try_acquire_owned().unwrap();
        let response = handle_post(
            State(state.clone()),
            post_request(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#, Some(&id)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(state.connections.lock().unwrap().is_empty());
        assert!(
            agent
                .tx
                .try_send_serialized(r#"{"jsonrpc":"2.0","method":"notice"}"#)
                .is_err()
        );
    }

    #[tokio::test]
    async fn legacy_component_fails_closed_without_a_bridge() {
        let state = BoundedAcpHttpServer::new(
            || {
                let (channel, peer) = agent_client_protocol::Channel::duplex();
                drop(peer);
                channel
            },
            ServerLimits::default(),
        )
        .unwrap()
        .state;
        let response = timeout(
            Duration::from_secs(2),
            handle_post(
                State(state.clone()),
                post_request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#, None),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(state.connections.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn bounded_router_rejects_websocket_upgrade() {
        let (state, _agent) = registry(ServerLimits::default());
        let request = Request::builder()
            .header(header::UPGRADE, "websocket")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            get_inner(state, request).unwrap_err(),
            StatusCode::NOT_IMPLEMENTED
        );
    }
}
