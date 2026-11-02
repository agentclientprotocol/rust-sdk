use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex as StdMutex, Weak},
    task::Poll,
};

use agent_client_protocol::{Channel, RawJsonRpcId, RawJsonRpcMessage, TransportFrame};
use futures::{FutureExt, SinkExt, StreamExt};
use tokio::sync::{Mutex, RwLock, mpsc, watch};
use tracing::{debug, error, trace};

use crate::protocol::session_id_from_message;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResponseRoute {
    Connection,
    Session(String),
}

enum OutboundTransport {
    Http(HttpOutbound),
    WebSocket(WebSocketOutbound),
}

/// The most session streams a client may hold open for sessions this
/// connection does not know yet.
///
/// A client may open a session stream before the `session/load` or
/// `session/resume` request that makes the session known, so the stream sees
/// the session's replayed updates. Such a stream stays unconfirmed until the
/// agent names the session. Beyond this limit, a GET for an unknown session
/// gets `404 Not Found`.
pub(crate) const MAX_UNCONFIRMED_SESSION_STREAMS: usize = 16;

struct HttpOutbound {
    connection_stream: OutboundMailbox,
    session_streams: Arc<StdMutex<SessionStreams>>,
    pending_routes: Mutex<HashMap<RawJsonRpcId, VecDeque<PendingRoute>>>,
}

/// Session streams of one HTTP connection, keyed by session ID.
#[derive(Default)]
struct SessionStreams {
    streams: HashMap<String, SessionStream>,
}

struct SessionStream {
    mailbox: Arc<OutboundMailbox>,
    /// Whether the agent has named this session on the connection.
    ///
    /// The agent names a session when it answers `session/new` or
    /// `session/fork` with its `sessionId`, answers a request scoped to it with
    /// a result, or sends a request or notification carrying it.
    confirmed: bool,
}

/// Why a session-scoped GET could not take its stream.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionStreamUnavailable {
    /// Another subscriber already holds the stream.
    Busy,
    /// The session is unknown to this connection and no unconfirmed stream
    /// slot is free.
    Unknown,
    /// The connection has no per-session streams (WebSocket).
    NotHttp,
}

/// How a request's response is routed, and what it does to the connection's
/// known sessions when it succeeds.
struct PendingRoute {
    route: ResponseRoute,
    effect: SessionEffect,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionEffect {
    None,
    /// A result names a new session in `sessionId` (`session/new`,
    /// `session/fork`).
    Opens,
    /// A result ends the session the request is scoped to (`session/close`,
    /// `session/delete`).
    Closes,
}

impl SessionEffect {
    fn for_method(method: &str) -> Self {
        match method {
            "session/new" | "session/fork" => Self::Opens,
            "session/close" | "session/delete" => Self::Closes,
            _ => Self::None,
        }
    }
}

struct WebSocketOutbound {
    all_outbound: OutboundMailbox,
}

struct OutboundMailbox {
    sender: mpsc::UnboundedSender<String>,
    receiver_slot: Arc<StdMutex<Option<mpsc::UnboundedReceiver<String>>>>,
}

pub(crate) struct OutboundLease {
    receiver: Option<mpsc::UnboundedReceiver<String>>,
    receiver_slot: Arc<StdMutex<Option<mpsc::UnboundedReceiver<String>>>>,
    session: Option<SessionLease>,
}

/// Ties a session stream lease to its entry in [`SessionStreams`].
struct SessionLease {
    streams: Weak<StdMutex<SessionStreams>>,
    session_id: String,
    /// Keeps the sender alive while the stream is open, even if the session's
    /// entry is removed (for example after `session/close`).
    mailbox: Arc<OutboundMailbox>,
}

impl OutboundMailbox {
    fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            sender,
            receiver_slot: Arc::new(StdMutex::new(Some(receiver))),
        }
    }

    fn push(&self, msg: String) -> Result<(), &'static str> {
        self.sender
            .send(msg)
            .map_err(|_| "outbound mailbox receiver closed")
    }

    fn try_acquire(&self) -> Option<OutboundLease> {
        let receiver = self
            .receiver_slot
            .lock()
            .expect("outbound mailbox receiver lock poisoned")
            .take()?;
        Some(OutboundLease {
            receiver: Some(receiver),
            receiver_slot: self.receiver_slot.clone(),
            session: None,
        })
    }

    fn is_leased(&self) -> bool {
        self.receiver_slot
            .lock()
            .expect("outbound mailbox receiver lock poisoned")
            .is_none()
    }
}

impl OutboundLease {
    pub(crate) async fn recv(&mut self) -> Option<String> {
        self.receiver
            .as_mut()
            .expect("outbound lease receiver missing")
            .recv()
            .await
    }

    pub(crate) fn try_recv(&mut self) -> Result<String, mpsc::error::TryRecvError> {
        self.receiver
            .as_mut()
            .expect("outbound lease receiver missing")
            .try_recv()
    }
}

impl Drop for OutboundLease {
    fn drop(&mut self) {
        let Some(receiver) = self.receiver.take() else {
            return;
        };
        let mut receiver_slot = self
            .receiver_slot
            .lock()
            .expect("outbound mailbox receiver lock poisoned");
        debug_assert!(receiver_slot.is_none());
        *receiver_slot = Some(receiver);
        drop(receiver_slot);

        // An unconfirmed session stream only exists while a client holds it.
        if let Some(session) = self.session.take()
            && let Some(streams) = session.streams.upgrade()
        {
            streams
                .lock()
                .expect("session streams lock poisoned")
                .release_unconfirmed(&session.session_id, &session.mailbox);
        }
    }
}

pub(crate) struct Connection {
    inbound_tx: mpsc::UnboundedSender<TransportFrame>,
    outbound_rx: Mutex<Option<mpsc::UnboundedReceiver<TransportFrame>>>,
    agent_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    router_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    closed_tx: watch::Sender<bool>,
    outbound_transport: OutboundTransport,
}

impl Connection {
    pub(crate) fn send_frame_to_agent(&self, frame: TransportFrame) -> Result<(), &'static str> {
        self.inbound_tx
            .send(frame)
            .map_err(|_| "agent channel closed")
    }

    pub(crate) async fn record_pending_route(
        &self,
        id: RawJsonRpcId,
        route: ResponseRoute,
        method: &str,
    ) {
        self.outbound_transport
            .record_pending_route(id, route, method)
            .await;
    }

    pub(crate) fn subscribe_connection_stream(&self) -> Option<OutboundLease> {
        self.outbound_transport.subscribe_connection_stream()
    }

    pub(crate) fn subscribe_session_stream(
        &self,
        session_id: &str,
    ) -> Result<OutboundLease, SessionStreamUnavailable> {
        self.outbound_transport.subscribe_session_stream(session_id)
    }

    pub(crate) fn subscribe_all_outbound(&self) -> Option<OutboundLease> {
        self.outbound_transport.subscribe_all_outbound()
    }

    pub(crate) fn subscribe_closed(&self) -> watch::Receiver<bool> {
        self.closed_tx.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn push_connection_stream_for_test(&self, msg: String) -> Result<(), &'static str> {
        self.outbound_transport.push_connection_stream_for_test(msg)
    }

    #[cfg(test)]
    pub(crate) fn session_stream_count_for_test(&self) -> usize {
        let OutboundTransport::Http(http) = &self.outbound_transport else {
            return 0;
        };
        http.session_streams
            .lock()
            .expect("session streams lock poisoned")
            .streams
            .len()
    }

    #[cfg(test)]
    pub(crate) fn push_all_outbound_for_test(&self, msg: String) -> Result<(), &'static str> {
        let OutboundTransport::WebSocket(websocket) = &self.outbound_transport else {
            return Err("not a WebSocket connection");
        };
        websocket.all_outbound.push(msg)
    }

    pub(crate) async fn start_router(self: &Arc<Self>) {
        let Some(mut rx) = self.outbound_rx.lock().await.take() else {
            return;
        };

        let connection = self.clone();
        *self.router_handle.lock().await = Some(tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Err(error) = connection.route_outbound(msg).await {
                    error!("{error}; closing connection streams");
                    connection.close_streams();
                    break;
                }
            }
        }));
    }

    pub(crate) async fn route_outbound(&self, frame: TransportFrame) -> Result<(), &'static str> {
        self.outbound_transport.route_outbound(frame).await
    }

    pub(crate) async fn recv_initial(&self) -> Option<TransportFrame> {
        let mut guard = self.outbound_rx.lock().await;
        let rx = guard.as_mut()?;
        rx.recv().await
    }

    pub(crate) async fn shutdown(&self) {
        // Explicit peer teardown is abortive. Natural agent completion instead
        // awaits its router before unregistering and closing streams.
        self.close_streams();
        if let Some(h) = self.agent_handle.lock().await.take() {
            h.abort();
        }
        if let Some(h) = self.router_handle.lock().await.take() {
            h.abort();
        }
    }

    fn close_streams(&self) {
        self.closed_tx.send_replace(true);
    }
}

impl OutboundTransport {
    fn http() -> Self {
        Self::Http(HttpOutbound::new())
    }

    fn websocket() -> Self {
        Self::WebSocket(WebSocketOutbound::new())
    }

    async fn record_pending_route(&self, id: RawJsonRpcId, route: ResponseRoute, method: &str) {
        let Self::Http(http) = self else {
            return;
        };

        http.record_pending_route(id, route, method).await;
    }

    fn subscribe_connection_stream(&self) -> Option<OutboundLease> {
        match self {
            Self::Http(http) => http.connection_stream.try_acquire(),
            Self::WebSocket(_) => None,
        }
    }

    fn subscribe_session_stream(
        &self,
        session_id: &str,
    ) -> Result<OutboundLease, SessionStreamUnavailable> {
        match self {
            Self::Http(http) => http.subscribe_session_stream(session_id),
            Self::WebSocket(_) => Err(SessionStreamUnavailable::NotHttp),
        }
    }

    fn subscribe_all_outbound(&self) -> Option<OutboundLease> {
        match self {
            Self::Http(_) => None,
            Self::WebSocket(websocket) => websocket.all_outbound.try_acquire(),
        }
    }

    #[cfg(test)]
    fn push_connection_stream_for_test(&self, msg: String) -> Result<(), &'static str> {
        let Self::Http(http) = self else {
            return Err("not an HTTP connection");
        };

        http.connection_stream.push(msg)
    }

    async fn route_outbound(&self, frame: TransportFrame) -> Result<(), &'static str> {
        match frame {
            TransportFrame::Single(message) => {
                let Ok(serialized) = serde_json::to_string(&message) else {
                    error!("failed to serialize outbound JSON-RPC message");
                    return Err("failed to serialize outbound JSON-RPC message");
                };
                match self {
                    Self::Http(http) => http.route_outbound(&message, serialized).await,
                    Self::WebSocket(websocket) => websocket.all_outbound.push(serialized),
                }
            }
            // Malformed and any other non-message frames are sent verbatim.
            frame => {
                let Ok(raw) = frame.to_json() else {
                    error!("failed to serialize outbound transport frame");
                    return Err("failed to serialize outbound transport frame");
                };
                match self {
                    Self::Http(http) => http.connection_stream.push(raw),
                    Self::WebSocket(websocket) => websocket.all_outbound.push(raw),
                }
            }
        }
    }
}

impl HttpOutbound {
    fn new() -> Self {
        Self {
            connection_stream: OutboundMailbox::new(),
            session_streams: Arc::new(StdMutex::new(SessionStreams::default())),
            pending_routes: Mutex::new(HashMap::new()),
        }
    }

    async fn record_pending_route(&self, id: RawJsonRpcId, route: ResponseRoute, method: &str) {
        if let Some(key) = pending_route_key(&id) {
            self.pending_routes
                .lock()
                .await
                .entry(key)
                .or_default()
                .push_back(PendingRoute {
                    route,
                    effect: SessionEffect::for_method(method),
                });
        }
    }

    fn session_streams(&self) -> std::sync::MutexGuard<'_, SessionStreams> {
        self.session_streams
            .lock()
            .expect("session streams lock poisoned")
    }

    /// Lease the stream of a session this connection knows, or of an unknown
    /// session while fewer than [`MAX_UNCONFIRMED_SESSION_STREAMS`] unconfirmed
    /// streams are open.
    fn subscribe_session_stream(
        &self,
        session_id: &str,
    ) -> Result<OutboundLease, SessionStreamUnavailable> {
        let mut streams = self.session_streams();
        let mailbox = if let Some(stream) = streams.streams.get(session_id) {
            stream.mailbox.clone()
        } else {
            if streams.unconfirmed_count() >= MAX_UNCONFIRMED_SESSION_STREAMS {
                return Err(SessionStreamUnavailable::Unknown);
            }
            let mailbox = Arc::new(OutboundMailbox::new());
            streams.streams.insert(
                session_id.to_string(),
                SessionStream {
                    mailbox: mailbox.clone(),
                    confirmed: false,
                },
            );
            mailbox
        };
        drop(streams);

        let mut lease = mailbox
            .try_acquire()
            .ok_or(SessionStreamUnavailable::Busy)?;
        lease.session = Some(SessionLease {
            streams: Arc::downgrade(&self.session_streams),
            session_id: session_id.to_string(),
            mailbox,
        });
        Ok(lease)
    }

    async fn route_outbound(
        &self,
        msg: &RawJsonRpcMessage,
        serialized: String,
    ) -> Result<(), &'static str> {
        let target = match msg {
            RawJsonRpcMessage::Request(_) | RawJsonRpcMessage::Notification(_) => {
                // The agent names the session, so it exists on this connection.
                session_id_from_message(msg)
                    .map(|session_id| self.session_streams().confirm(&session_id))
            }
            RawJsonRpcMessage::Response(response) => {
                let pending = match msg.response_id().and_then(pending_route_key) {
                    Some(key) => {
                        let mut pending_routes = self.pending_routes.lock().await;
                        take_pending_route(&mut pending_routes, &key)
                    }
                    None => None,
                };
                pending.and_then(|pending| self.response_target(response, pending))
            }
        };

        match target {
            None => {
                trace!(target = "connection", "→ connection-scoped stream");
                self.connection_stream.push(serialized)
            }
            Some(mailbox) => {
                trace!(target = "session", "→ session-scoped stream");
                mailbox.push(serialized)
            }
        }
    }

    /// Update the known sessions for a response and pick its session stream.
    ///
    /// `None` routes the response to the connection stream. A response scoped
    /// to a session with no stream goes there too, so a POST naming an
    /// unknown session allocates nothing and its error still reaches the
    /// client.
    fn response_target(
        &self,
        response: &agent_client_protocol::RawJsonRpcResponse,
        pending: PendingRoute,
    ) -> Option<Arc<OutboundMailbox>> {
        let result = match response {
            agent_client_protocol::RawJsonRpcResponse::Result { result, .. } => Some(result),
            agent_client_protocol::RawJsonRpcResponse::Error { .. } => None,
        };
        let mut streams = self.session_streams();

        // Record a created session before its response is queued, so a GET
        // sent right after the client reads the response finds it.
        if pending.effect == SessionEffect::Opens
            && let Some(session_id) = result
                .and_then(|result| result.get("sessionId"))
                .and_then(serde_json::Value::as_str)
        {
            streams.confirm(session_id);
        }

        let ResponseRoute::Session(session_id) = pending.route else {
            return None;
        };
        let mut target = streams
            .streams
            .get(&session_id)
            .map(|stream| stream.mailbox.clone());
        if result.is_some() {
            match pending.effect {
                SessionEffect::Closes => {
                    // A queued response would be lost with the removed
                    // mailbox, so send it on the connection stream unless a
                    // client is reading the session stream.
                    if target.as_ref().is_some_and(|mailbox| !mailbox.is_leased()) {
                        target = None;
                    }
                    streams.streams.remove(&session_id);
                }
                SessionEffect::None | SessionEffect::Opens => {
                    streams.confirm(&session_id);
                }
            }
        }
        target
    }
}

impl SessionStreams {
    /// Mark a session as known, creating its stream if needed.
    fn confirm(&mut self, session_id: &str) -> Arc<OutboundMailbox> {
        let stream = self
            .streams
            .entry(session_id.to_string())
            .or_insert_with(|| SessionStream {
                mailbox: Arc::new(OutboundMailbox::new()),
                confirmed: false,
            });
        stream.confirmed = true;
        stream.mailbox.clone()
    }

    fn unconfirmed_count(&self) -> usize {
        self.streams
            .values()
            .filter(|stream| !stream.confirmed)
            .count()
    }

    /// Forget an unconfirmed session stream once its subscriber leaves.
    fn release_unconfirmed(&mut self, session_id: &str, mailbox: &Arc<OutboundMailbox>) {
        if self
            .streams
            .get(session_id)
            .is_some_and(|stream| !stream.confirmed && Arc::ptr_eq(&stream.mailbox, mailbox))
        {
            self.streams.remove(session_id);
        }
    }
}

impl WebSocketOutbound {
    fn new() -> Self {
        Self {
            all_outbound: OutboundMailbox::new(),
        }
    }
}

pub(crate) struct ConnectionRegistry {
    factory: Arc<dyn AgentFactory>,
    connections: Arc<RwLock<HashMap<String, Arc<Connection>>>>,
}

pub(crate) trait AgentFactory: Send + Sync + 'static {
    fn spawn_agent(&self) -> (Channel, Option<agent_client_protocol::ConnectionDriver>);
}

impl<F, C> AgentFactory for F
where
    F: Fn() -> C + Send + Sync + 'static,
    C: agent_client_protocol::ConnectTo<agent_client_protocol::Client>,
{
    fn spawn_agent(&self) -> (Channel, Option<agent_client_protocol::ConnectionDriver>) {
        self().into_channel_and_future()
    }
}

impl ConnectionRegistry {
    pub(crate) fn new(factory: Arc<dyn AgentFactory>) -> Self {
        Self {
            factory,
            connections: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub(crate) fn next_connection_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    pub(crate) async fn create_connection(&self) -> (String, Arc<Connection>) {
        let connection_id = Self::next_connection_id();
        let connection = self.create_connection_with_id(connection_id.clone()).await;
        (connection_id, connection)
    }

    pub(crate) async fn create_connection_with_id(&self, connection_id: String) -> Arc<Connection> {
        self.create_connection_with_transport(connection_id, OutboundTransport::http())
            .await
    }

    pub(crate) async fn create_websocket_connection_with_id(
        &self,
        connection_id: String,
    ) -> Arc<Connection> {
        self.create_connection_with_transport(connection_id, OutboundTransport::websocket())
            .await
    }

    async fn create_connection_with_transport(
        &self,
        connection_id: String,
        outbound_transport: OutboundTransport,
    ) -> Arc<Connection> {
        let (channel, agent_future) = self.factory.spawn_agent();
        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<TransportFrame>();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<TransportFrame>();
        let (closed_tx, _) = watch::channel(false);

        let Channel {
            rx: mut agent_rx,
            tx: mut agent_tx,
            ..
        } = channel;
        let inbound = async move {
            while let Some(msg) = inbound_rx.recv().await {
                if agent_tx.send(msg).await.is_err() {
                    break;
                }
            }
            drop(agent_tx.close().await);
        };
        let (inbound_abort, inbound_abort_registration) = futures::future::AbortHandle::new_pair();
        let inbound = futures::future::Abortable::new(inbound, inbound_abort_registration);
        let inbound_abort_for_outbound = inbound_abort.clone();
        let (finish_outbound_tx, finish_outbound_rx) = futures::channel::oneshot::channel::<()>();
        let outbound = async move {
            let mut finish_outbound_rx = Some(finish_outbound_rx);
            while let Some(msg) = futures::future::poll_fn(|cx| {
                if let Some(finish) = &mut finish_outbound_rx
                    && let Poll::Ready(result) = finish.poll_unpin(cx)
                {
                    finish_outbound_rx = None;
                    if result.is_ok() {
                        // Active completion rejects escaped producers while
                        // leaving already accepted frames available to drain.
                        agent_rx.close();
                    }
                }
                agent_rx.poll_next_unpin(cx)
            })
            .await
            {
                if outbound_tx.send(msg).is_err() {
                    inbound_abort_for_outbound.abort();
                    break;
                }
            }
        };
        let pump = async move {
            let (_inbound_result, ()) = futures::join!(inbound, outbound);
        };

        let connection = Arc::new(Connection {
            inbound_tx,
            outbound_rx: Mutex::new(Some(outbound_rx)),
            agent_handle: Mutex::new(None),
            router_handle: Mutex::new(None),
            closed_tx,
            outbound_transport,
        });

        self.connections
            .write()
            .await
            .insert(connection_id.clone(), connection.clone());

        let conn_id_for_task = connection_id.clone();
        let connections = self.connections.clone();
        let connection_for_task = Arc::downgrade(&connection);
        let agent_handle = tokio::spawn(async move {
            let conn_id_for_agent = conn_id_for_task.clone();
            if let Some(agent_future) = agent_future {
                let agent = async move {
                    if agent_future.await.is_err() {
                        error!(connection_id = %conn_id_for_agent, "ACP agent task failed");
                    }
                };
                futures::pin_mut!(agent);
                futures::pin_mut!(pump);
                match futures::future::select(agent, pump).await {
                    futures::future::Either::Left(((), pump)) => {
                        inbound_abort.abort();
                        let _sent = finish_outbound_tx.send(());
                        pump.await;
                    }
                    futures::future::Either::Right(((), _agent)) => {}
                }
            } else {
                // With no owned agent work, only the channel pumps determine
                // completion. Do not interpret absence as connection EOF.
                drop(finish_outbound_tx);
                pump.await;
            }
            debug!(connection_id = %conn_id_for_task, "ACP connection task ended");
            let connection_to_close = drain_connection_router(connection_for_task).await;
            connections.write().await.remove(&conn_id_for_task);
            if let Some(connection) = connection_to_close {
                connection.close_streams();
            }
        });

        *connection.agent_handle.lock().await = Some(agent_handle);

        connection
    }

    pub(crate) async fn get(&self, connection_id: &str) -> Option<Arc<Connection>> {
        self.connections.read().await.get(connection_id).cloned()
    }

    pub(crate) async fn remove(&self, connection_id: &str) -> Option<Arc<Connection>> {
        self.connections.write().await.remove(connection_id)
    }

    #[cfg(test)]
    pub(crate) async fn len(&self) -> usize {
        self.connections.read().await.len()
    }
}

// Taking a JoinHandle transfers cancellation ownership out of the connection.
// Keep that ownership through the await: dropping a bare handle detaches it.
struct AbortTakenRouterOnDrop(tokio::task::AbortHandle);

impl Drop for AbortTakenRouterOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn drain_connection_router(connection: Weak<Connection>) -> Option<Arc<Connection>> {
    let connection = connection.upgrade()?;
    let router_handle = connection.router_handle.lock().await.take();
    if let Some(handle) = router_handle {
        let _abort_on_drop = AbortTakenRouterOnDrop(handle.abort_handle());
        if handle.await.is_err() {
            error!("outbound router task failed while draining");
        }
    }
    Some(connection)
}

fn pending_route_key(id: &RawJsonRpcId) -> Option<RawJsonRpcId> {
    match id {
        RawJsonRpcId::Null => None,
        RawJsonRpcId::Number(_) | RawJsonRpcId::Str(_) => Some(id.clone()),
    }
}

fn take_pending_route(
    pending_routes: &mut HashMap<RawJsonRpcId, VecDeque<PendingRoute>>,
    key: &RawJsonRpcId,
) -> Option<PendingRoute> {
    let routes = pending_routes.get_mut(key)?;
    let route = routes.pop_front();
    let remove_entry = routes.is_empty();
    if remove_entry {
        pending_routes.remove(key);
    }
    route
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_client_protocol::ConnectionDriver;
    use tokio::{
        sync::Notify,
        time::{Duration, sleep, timeout},
    };

    use super::*;

    const ISSUE_288_BURST: usize = 1_025;

    #[tokio::test]
    async fn outbound_mailbox_buffers_bursts_before_subscription() {
        let mailbox = OutboundMailbox::new();

        for index in 0..ISSUE_288_BURST {
            mailbox.push(format!("message-{index}")).unwrap();
        }

        let mut receiver = mailbox.try_acquire().unwrap();
        for index in 0..ISSUE_288_BURST {
            assert_eq!(
                receiver.recv().await,
                Some(format!("message-{index}")),
                "message {index} should remain ordered"
            );
        }
    }

    #[tokio::test]
    async fn outbound_mailbox_does_not_stall_when_subscriber_is_slow() {
        let mailbox = OutboundMailbox::new();
        let mut receiver = mailbox.try_acquire().unwrap();

        for index in 0..ISSUE_288_BURST {
            mailbox.push(format!("message-{index}")).unwrap();
        }
        for index in 0..ISSUE_288_BURST {
            assert_eq!(
                receiver.recv().await,
                Some(format!("message-{index}")),
                "message {index} should remain ordered"
            );
        }
    }

    #[tokio::test]
    async fn outbound_mailbox_has_one_active_owner_and_preserves_queued_frames() {
        let mailbox = OutboundMailbox::new();
        let receiver = mailbox.try_acquire().unwrap();
        assert!(mailbox.try_acquire().is_none());
        mailbox
            .push("queued before disconnect".to_string())
            .unwrap();
        drop(receiver);

        mailbox.push("queued after disconnect".to_string()).unwrap();
        let mut resumed = mailbox.try_acquire().unwrap();
        assert_eq!(
            resumed.recv().await.as_deref(),
            Some("queued before disconnect")
        );
        assert_eq!(
            resumed.recv().await.as_deref(),
            Some("queued after disconnect")
        );
    }

    #[tokio::test]
    async fn slow_session_mailbox_does_not_stall_other_routes() {
        let outbound = HttpOutbound::new();
        let mut slow_session = outbound.subscribe_session_stream("slow-session").unwrap();
        let mut fast_session = outbound.subscribe_session_stream("fast-session").unwrap();

        timeout(Duration::from_secs(1), async {
            for index in 0..ISSUE_288_BURST {
                let message = RawJsonRpcMessage::notification(
                    "session/update".to_string(),
                    serde_json::json!({
                        "sessionId": "slow-session",
                        "index": index,
                    }),
                )
                .unwrap();
                let serialized = serde_json::to_string(&message).unwrap();
                outbound.route_outbound(&message, serialized).await.unwrap();
            }

            let marker = RawJsonRpcMessage::notification(
                "session/update".to_string(),
                serde_json::json!({
                    "sessionId": "fast-session",
                    "marker": true,
                }),
            )
            .unwrap();
            let serialized = serde_json::to_string(&marker).unwrap();
            outbound.route_outbound(&marker, serialized).await.unwrap();
        })
        .await
        .expect("a slow session must not stall routing to another session");

        let marker = timeout(Duration::from_secs(1), fast_session.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&marker).unwrap()["params"]["marker"],
            true
        );

        for index in 0..ISSUE_288_BURST {
            let message = slow_session.recv().await.unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&message).unwrap()["params"]["index"],
                index
            );
        }
    }

    struct ExitingAgentFactory {
        exit: Arc<Notify>,
    }

    impl AgentFactory for ExitingAgentFactory {
        fn spawn_agent(&self) -> (Channel, Option<ConnectionDriver>) {
            let (agent, transport) = Channel::duplex();
            let exit = self.exit.clone();
            let future = ConnectionDriver::new(async move {
                exit.notified().await;
                drop(agent);
                Ok(())
            });

            (transport, Some(future))
        }
    }

    struct RespondThenExitAgentFactory;

    impl AgentFactory for RespondThenExitAgentFactory {
        fn spawn_agent(&self) -> (Channel, Option<ConnectionDriver>) {
            let (agent, transport) = Channel::duplex();
            let future = ConnectionDriver::new(async move {
                agent
                    .tx
                    .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::response(
                        RawJsonRpcId::Number(1),
                        Ok(serde_json::json!({ "done": true })),
                    )))
                    .unwrap();
                Ok(())
            });

            (transport, Some(future))
        }
    }

    struct MalformedThenWaitAgentFactory {
        emit: Arc<Notify>,
    }

    impl AgentFactory for MalformedThenWaitAgentFactory {
        fn spawn_agent(&self) -> (Channel, Option<ConnectionDriver>) {
            let (agent, transport) = Channel::duplex();
            let emit = self.emit.clone();
            let future = ConnectionDriver::new(async move {
                emit.notified().await;
                agent
                    .tx
                    .unbounded_send(TransportFrame::Malformed {
                        raw: "{not json".to_string(),
                        error: agent_client_protocol::Error::parse_error()
                            .data("transport parse error"),
                    })
                    .unwrap();
                std::future::pending::<agent_client_protocol::Result<()>>().await
            });

            (transport, Some(future))
        }
    }

    struct SendThenWaitAgentFactory {
        message: RawJsonRpcMessage,
        exit: Arc<Notify>,
    }

    impl AgentFactory for SendThenWaitAgentFactory {
        fn spawn_agent(&self) -> (Channel, Option<ConnectionDriver>) {
            let (agent, transport) = Channel::duplex();
            let message = self.message.clone();
            let exit = self.exit.clone();
            let future = ConnectionDriver::new(async move {
                agent
                    .tx
                    .unbounded_send(TransportFrame::Single(message))
                    .unwrap();
                exit.notified().await;
                Ok(())
            });

            (transport, Some(future))
        }
    }

    struct FinalFrameThenExitAgentFactory {
        emit: Arc<Notify>,
        escaped_output:
            Arc<StdMutex<Option<futures::channel::mpsc::UnboundedSender<TransportFrame>>>>,
    }

    impl AgentFactory for FinalFrameThenExitAgentFactory {
        fn spawn_agent(&self) -> (Channel, Option<ConnectionDriver>) {
            let (agent, transport) = Channel::duplex();
            *self.escaped_output.lock().unwrap() = Some(agent.tx.clone());
            let emit = self.emit.clone();
            let future = ConnectionDriver::new(async move {
                emit.notified().await;
                agent
                    .tx
                    .unbounded_send(TransportFrame::Single(
                        RawJsonRpcMessage::notification(
                            "test/final".to_string(),
                            serde_json::json!({}),
                        )
                        .unwrap(),
                    ))
                    .unwrap();
                Ok(())
            });

            (transport, Some(future))
        }
    }

    #[tokio::test]
    async fn absent_agent_driver_preserves_half_closes_and_natural_completion() {
        let (endpoint, mut remote) = Channel::duplex();
        let endpoint = std::sync::Mutex::new(Some(endpoint));
        let registry = ConnectionRegistry::new(Arc::new(move || {
            endpoint.lock().unwrap().take().expect("one connection")
        }));
        let (connection_id, connection) = registry.create_connection().await;
        let frame = TransportFrame::Single(
            RawJsonRpcMessage::notification("test/passive".into(), serde_json::json!({})).unwrap(),
        );
        connection.inbound_tx.send(frame.clone()).unwrap();
        assert!(
            timeout(Duration::from_secs(1), remote.rx.next())
                .await
                .expect("absent owned work must not abort inbound forwarding")
                .is_some()
        );
        remote.tx.unbounded_send(frame.clone()).unwrap();
        assert!(
            timeout(Duration::from_secs(1), connection.recv_initial())
                .await
                .expect("passive endpoint must keep its reverse direction alive")
                .is_some()
        );
        assert!(registry.get(&connection_id).await.is_some());
        assert!(!*connection.subscribe_closed().borrow());

        drop(remote.tx);
        assert!(
            timeout(Duration::from_secs(1), connection.recv_initial())
                .await
                .expect("the outbound pump should observe its half-close")
                .is_none()
        );
        connection.inbound_tx.send(frame.clone()).unwrap();
        assert!(
            timeout(Duration::from_secs(1), remote.rx.next())
                .await
                .expect("the other half must still forward after outbound EOF")
                .is_some()
        );
        assert!(registry.get(&connection_id).await.is_some());
        let mut closed = connection.subscribe_closed();
        assert!(!*closed.borrow());

        drop(remote.rx);
        // Forwarding observes the closed recipient on its next send; no
        // explicit registry shutdown is needed to finish the two pumps.
        connection.inbound_tx.send(frame).unwrap();
        timeout(Duration::from_secs(1), async {
            while !*closed.borrow() {
                closed.changed().await.unwrap();
            }
        })
        .await
        .expect("both closed halves should finish without a synthetic driver");
        assert!(registry.get(&connection_id).await.is_none());
    }

    #[tokio::test]
    async fn agent_exit_removes_connection_and_closes_streams() {
        let exit = Arc::new(Notify::new());
        let registry =
            ConnectionRegistry::new(Arc::new(ExitingAgentFactory { exit: exit.clone() }));
        let (connection_id, connection) = registry.create_connection().await;

        assert!(registry.get(&connection_id).await.is_some());

        exit.notify_one();
        timeout(Duration::from_secs(1), async {
            loop {
                if registry.get(&connection_id).await.is_none() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        assert!(*connection.subscribe_closed().borrow());
    }

    #[tokio::test]
    async fn malformed_frame_is_relayed_without_closing_connection() {
        let emit = Arc::new(Notify::new());
        let registry = ConnectionRegistry::new(Arc::new(MalformedThenWaitAgentFactory {
            emit: emit.clone(),
        }));
        let (connection_id, connection) = registry.create_connection().await;
        let mut outbound = connection.subscribe_connection_stream().unwrap();

        assert!(registry.get(&connection_id).await.is_some());

        connection.start_router().await;
        emit.notify_one();

        let raw = timeout(Duration::from_secs(1), outbound.recv())
            .await
            .unwrap()
            .expect("malformed frame should be relayed");
        assert_eq!(raw, "{not json");
        assert!(registry.get(&connection_id).await.is_some());
        assert!(!*connection.subscribe_closed().borrow());

        registry.remove(&connection_id).await;
        connection.shutdown().await;
    }

    #[tokio::test]
    async fn agent_exit_drains_buffered_outbound_messages() {
        let registry = ConnectionRegistry::new(Arc::new(RespondThenExitAgentFactory));
        let (connection_id, connection) = registry.create_connection().await;

        let frame = timeout(Duration::from_secs(1), connection.recv_initial())
            .await
            .unwrap()
            .expect("buffered response should be forwarded before teardown");

        assert!(matches!(
            frame,
            TransportFrame::Single(RawJsonRpcMessage::Response(
                agent_client_protocol::RawJsonRpcResponse::Result {
                    id: RawJsonRpcId::Number(1),
                    ..
                }
            ))
        ));
        timeout(Duration::from_secs(1), async {
            loop {
                if registry.get(&connection_id).await.is_none() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(*connection.subscribe_closed().borrow());
    }

    #[tokio::test]
    async fn agent_exit_flushes_final_frame_before_closing_streams() {
        let emit = Arc::new(Notify::new());
        let escaped_output = Arc::new(StdMutex::new(None));
        let registry = ConnectionRegistry::new(Arc::new(FinalFrameThenExitAgentFactory {
            emit: emit.clone(),
            escaped_output: escaped_output.clone(),
        }));
        let (connection_id, connection) = registry.create_connection().await;
        let mut outbound = connection.subscribe_connection_stream().unwrap();
        connection.start_router().await;
        let escaped_output = escaped_output.lock().unwrap().take().unwrap();

        emit.notify_one();
        timeout(Duration::from_secs(1), async {
            let mut closed = connection.subscribe_closed();
            while !*closed.borrow() {
                closed.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(registry.get(&connection_id).await.is_none());

        let text = timeout(Duration::from_secs(1), outbound.recv())
            .await
            .unwrap()
            .expect("final frame should remain queued after stream closure");
        let message = serde_json::from_str::<RawJsonRpcMessage>(&text).unwrap();
        assert!(matches!(
            message,
            RawJsonRpcMessage::Notification(notification)
                if notification.method.as_ref() == "test/final"
        ));
        assert!(
            escaped_output
                .unbounded_send(TransportFrame::Single(
                    RawJsonRpcMessage::notification("test/late".into(), serde_json::json!({}))
                        .unwrap(),
                ))
                .is_err(),
            "active completion must reject escaped senders without dropping them"
        );
        // Stream termination is signalled by closed_tx; the connection keeps
        // its mailbox sender alive so later subscribers can drain queued data.
        assert_eq!(outbound.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    }

    #[tokio::test]
    async fn active_agent_remains_registered_until_outbound_router_drains() {
        let emit = Arc::new(Notify::new());
        let registry = ConnectionRegistry::new(Arc::new(FinalFrameThenExitAgentFactory {
            emit: emit.clone(),
            escaped_output: Arc::new(StdMutex::new(None)),
        }));
        let (connection_id, connection) = registry.create_connection().await;
        let mut outbound = connection.subscribe_connection_stream().unwrap();
        let mut frames = connection.outbound_rx.lock().await.take().unwrap();
        let (routing_started_tx, routing_started_rx) = tokio::sync::oneshot::channel();
        let (release_router_tx, release_router_rx) = tokio::sync::oneshot::channel();
        let routing_connection = connection.clone();
        *connection.router_handle.lock().await = Some(tokio::spawn(async move {
            let frame = frames.recv().await.expect("accepted final frame");
            let _sent = routing_started_tx.send(());
            // A dropped release sender also unblocks failed-test cleanup.
            let _released = release_router_rx.await;
            routing_connection.route_outbound(frame).await.unwrap();
            while let Some(frame) = frames.recv().await {
                routing_connection.route_outbound(frame).await.unwrap();
            }
        }));

        emit.notify_one();
        timeout(Duration::from_secs(1), async {
            routing_started_rx.await.unwrap();
            // Taking the join handle establishes that natural shutdown has
            // reached router drain, not merely that the router is scheduled.
            while connection.router_handle.lock().await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("natural shutdown should await the gated router");
        assert!(
            registry.get(&connection_id).await.is_some(),
            "the connection must remain discoverable until accepted output is routed"
        );
        let mut closed = connection.subscribe_closed();
        assert!(!*closed.borrow());

        release_router_tx.send(()).unwrap();
        timeout(Duration::from_secs(1), async {
            while !*closed.borrow() {
                closed.changed().await.unwrap();
            }
        })
        .await
        .expect("closure should follow router drain and registry removal");
        assert!(registry.get(&connection_id).await.is_none());
        let text = outbound.try_recv().expect("the final frame must be routed");
        let message = serde_json::from_str::<RawJsonRpcMessage>(&text).unwrap();
        assert!(matches!(
            message,
            RawJsonRpcMessage::Notification(notification)
                if notification.method.as_ref() == "test/final"
        ));
    }

    struct RouterDropProbe(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for RouterDropProbe {
        fn drop(&mut self) {
            let _sent = self.0.take().unwrap().send(());
        }
    }

    #[tokio::test]
    async fn shutdown_during_natural_router_drain_cancels_owned_router() {
        let emit = Arc::new(Notify::new());
        let registry = ConnectionRegistry::new(Arc::new(FinalFrameThenExitAgentFactory {
            emit: emit.clone(),
            escaped_output: Arc::new(StdMutex::new(None)),
        }));
        let (connection_id, connection) = registry.create_connection().await;
        let weak_connection = Arc::downgrade(&connection);
        let mut outbound = connection.subscribe_connection_stream().unwrap();
        let mut closed = connection.subscribe_closed();
        let mut frames = connection.outbound_rx.lock().await.take().unwrap();
        let (routing_started_tx, routing_started_rx) = tokio::sync::oneshot::channel();
        let (release_router_tx, release_router_rx) = tokio::sync::oneshot::channel::<()>();
        let (dropped_tx, mut dropped_rx) = tokio::sync::oneshot::channel();
        let routing_connection = connection.clone();
        let router = tokio::spawn(async move {
            let _drop_probe = RouterDropProbe(Some(dropped_tx));
            let frame = frames.recv().await.expect("accepted final frame");
            routing_started_tx.send(()).unwrap();
            let _released = release_router_rx.await;
            routing_connection.route_outbound(frame).await.unwrap();
            while let Some(frame) = frames.recv().await {
                routing_connection.route_outbound(frame).await.unwrap();
            }
        });
        // Keep an independent abort handle solely to clean up a failing probe.
        let failed_test_cleanup = router.abort_handle();
        *connection.router_handle.lock().await = Some(router);

        emit.notify_one();
        timeout(Duration::from_secs(1), async {
            routing_started_rx.await.unwrap();
            while connection.router_handle.lock().await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("natural cleanup must have taken the router join handle");
        assert!(registry.get(&connection_id).await.is_some());
        assert!(!*closed.borrow());

        // Match DELETE: remove the discoverable connection and shut it down.
        let removed = registry.remove(&connection_id).await.unwrap();
        removed.shutdown().await;
        closed.changed().await.unwrap();
        assert!(*closed.borrow());
        // Raw mailbox EOF is not the stream-closure contract.
        assert_eq!(outbound.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        drop(removed);
        drop(connection);

        // The gate remains held throughout both observations. Only cancellation,
        // not releasing or dropping its sender, may finish the owned router.
        let router_cancelled = timeout(Duration::from_secs(1), &mut dropped_rx)
            .await
            .is_ok_and(|result| result.is_ok());
        let connection_released = timeout(Duration::from_secs(1), async {
            while weak_connection.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        if !router_cancelled {
            failed_test_cleanup.abort();
            timeout(Duration::from_secs(1), &mut dropped_rx)
                .await
                .expect("failed-test cleanup must cancel the orphan")
                .unwrap();
        }
        drop(release_router_tx);
        assert!(
            router_cancelled && connection_released,
            "shutdown must cancel the gated owned router without releasing its gate: router_cancelled={router_cancelled}, connection_released={connection_released}"
        );
    }

    #[tokio::test]
    async fn protocol_level_notification_routes_to_connection_stream() {
        let exit = Arc::new(Notify::new());
        let message = RawJsonRpcMessage::notification(
            "$/cancel_request".to_string(),
            serde_json::json!({
                "requestId": 1,
                "sessionId": "session-1"
            }),
        )
        .unwrap();
        let registry = ConnectionRegistry::new(Arc::new(SendThenWaitAgentFactory {
            message,
            exit: exit.clone(),
        }));
        let (_connection_id, connection) = registry.create_connection().await;
        let mut connection_rx = connection.subscribe_connection_stream().unwrap();
        let mut session_rx = connection.subscribe_session_stream("session-1").unwrap();

        connection.start_router().await;

        let text = timeout(Duration::from_secs(1), connection_rx.recv())
            .await
            .unwrap()
            .expect("protocol-level notification should reach connection stream");
        let routed = serde_json::from_str::<RawJsonRpcMessage>(&text).unwrap();
        assert!(matches!(
            routed,
            RawJsonRpcMessage::Notification(notification)
                if notification.method.as_ref() == "$/cancel_request"
        ));
        assert!(session_rx.try_recv().is_err());

        exit.notify_one();
        connection.shutdown().await;
    }

    #[tokio::test]
    async fn http_connection_does_not_expose_websocket_mailbox() {
        let exit = Arc::new(Notify::new());
        let message =
            RawJsonRpcMessage::notification("test/method".to_string(), serde_json::json!({}))
                .unwrap();
        let registry = ConnectionRegistry::new(Arc::new(SendThenWaitAgentFactory {
            message,
            exit: exit.clone(),
        }));
        let (_connection_id, connection) = registry.create_connection().await;
        let mut connection_rx = connection.subscribe_connection_stream().unwrap();

        connection.start_router().await;

        let text = timeout(Duration::from_secs(1), connection_rx.recv())
            .await
            .unwrap()
            .expect("message should reach HTTP connection stream");
        assert!(serde_json::from_str::<RawJsonRpcMessage>(&text).is_ok());

        assert!(connection.subscribe_all_outbound().is_none());

        exit.notify_one();
        connection.shutdown().await;
    }

    #[tokio::test]
    async fn websocket_connection_does_not_expose_http_mailboxes() {
        let exit = Arc::new(Notify::new());
        let message = RawJsonRpcMessage::notification(
            "test/method".to_string(),
            serde_json::json!({ "sessionId": "session-1" }),
        )
        .unwrap();
        let registry = ConnectionRegistry::new(Arc::new(SendThenWaitAgentFactory {
            message,
            exit: exit.clone(),
        }));
        let connection = registry
            .create_websocket_connection_with_id("conn-1".to_string())
            .await;
        let mut all_rx = connection.subscribe_all_outbound().unwrap();

        connection.start_router().await;

        let text = timeout(Duration::from_secs(1), all_rx.recv())
            .await
            .unwrap()
            .expect("message should reach WebSocket all-outbound stream");
        assert!(serde_json::from_str::<RawJsonRpcMessage>(&text).is_ok());

        assert!(connection.subscribe_connection_stream().is_none());
        assert_eq!(
            connection.subscribe_session_stream("session-1").err(),
            Some(SessionStreamUnavailable::NotHttp)
        );

        exit.notify_one();
        connection.shutdown().await;
    }

    fn session_count(outbound: &HttpOutbound) -> usize {
        outbound.session_streams().streams.len()
    }

    async fn route(outbound: &HttpOutbound, message: &RawJsonRpcMessage) {
        let serialized = serde_json::to_string(message).unwrap();
        outbound.route_outbound(message, serialized).await.unwrap();
    }

    fn result(id: i64, result: serde_json::Value) -> RawJsonRpcMessage {
        RawJsonRpcMessage::response(RawJsonRpcId::Number(id), Ok(result))
    }

    fn method_from(text: &str) -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(text).unwrap()
    }

    #[tokio::test]
    async fn unknown_session_streams_are_bounded_and_released() {
        let outbound = HttpOutbound::new();
        let mut leases = (0..MAX_UNCONFIRMED_SESSION_STREAMS)
            .map(|index| {
                outbound
                    .subscribe_session_stream(&format!("made-up-{index}"))
                    .unwrap()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            outbound.subscribe_session_stream("one-too-many").err(),
            Some(SessionStreamUnavailable::Unknown)
        );
        assert_eq!(session_count(&outbound), MAX_UNCONFIRMED_SESSION_STREAMS);

        // Leaving an unconfirmed stream forgets it and frees its slot.
        leases.pop();
        assert_eq!(
            session_count(&outbound),
            MAX_UNCONFIRMED_SESSION_STREAMS - 1
        );
        let _replacement = outbound.subscribe_session_stream("one-too-many").unwrap();
        drop(leases);
        assert_eq!(session_count(&outbound), 1);
    }

    #[tokio::test]
    async fn created_session_stream_is_known_before_its_response_is_queued() {
        let outbound = HttpOutbound::new();
        // Fill every unconfirmed slot so only known sessions can be opened.
        let _unknown = (0..MAX_UNCONFIRMED_SESSION_STREAMS)
            .map(|index| {
                outbound
                    .subscribe_session_stream(&format!("made-up-{index}"))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let mut connection_rx = outbound.connection_stream.try_acquire().unwrap();

        outbound
            .record_pending_route(
                RawJsonRpcId::Number(1),
                ResponseRoute::Connection,
                "session/new",
            )
            .await;
        route(
            &outbound,
            &result(1, serde_json::json!({ "sessionId": "created" })),
        )
        .await;
        let update = RawJsonRpcMessage::notification(
            "session/update".to_string(),
            serde_json::json!({ "sessionId": "created" }),
        )
        .unwrap();
        route(&outbound, &update).await;

        // The client reads the response, then opens the session stream.
        let response = connection_rx.recv().await.unwrap();
        assert_eq!(method_from(&response)["result"]["sessionId"], "created");
        let mut session_rx = outbound.subscribe_session_stream("created").unwrap();
        assert_eq!(
            method_from(&session_rx.recv().await.unwrap())["method"],
            "session/update"
        );

        // A known session survives its subscriber leaving.
        drop(session_rx);
        assert!(outbound.subscribe_session_stream("created").is_ok());
    }

    #[tokio::test]
    async fn session_scoped_request_for_unknown_session_allocates_no_stream() {
        let outbound = HttpOutbound::new();
        let mut connection_rx = outbound.connection_stream.try_acquire().unwrap();

        outbound
            .record_pending_route(
                RawJsonRpcId::Number(7),
                ResponseRoute::Session("made-up".to_string()),
                "session/prompt",
            )
            .await;
        assert_eq!(session_count(&outbound), 0);

        let error = RawJsonRpcMessage::response(
            RawJsonRpcId::Number(7),
            Err(agent_client_protocol::Error::invalid_params()),
        );
        route(&outbound, &error).await;

        // The error reaches the client on the connection stream.
        assert_eq!(session_count(&outbound), 0);
        assert_eq!(method_from(&connection_rx.recv().await.unwrap())["id"], 7);
    }

    #[tokio::test]
    async fn stream_opened_before_load_is_confirmed_by_its_result() {
        let outbound = HttpOutbound::new();
        let mut session_rx = outbound.subscribe_session_stream("loaded").unwrap();
        outbound
            .record_pending_route(
                RawJsonRpcId::Number(2),
                ResponseRoute::Session("loaded".to_string()),
                "session/load",
            )
            .await;
        let replay = RawJsonRpcMessage::notification(
            "session/update".to_string(),
            serde_json::json!({ "sessionId": "loaded" }),
        )
        .unwrap();
        route(&outbound, &replay).await;
        route(&outbound, &result(2, serde_json::json!({}))).await;

        // Replayed updates and the response arrive in order on the stream.
        assert_eq!(
            method_from(&session_rx.recv().await.unwrap())["method"],
            "session/update"
        );
        assert_eq!(method_from(&session_rx.recv().await.unwrap())["id"], 2);

        drop(session_rx);
        assert_eq!(session_count(&outbound), 1);
        assert!(outbound.subscribe_session_stream("loaded").is_ok());
    }

    #[tokio::test]
    async fn failed_load_leaves_session_unknown() {
        let outbound = HttpOutbound::new();
        let mut session_rx = outbound.subscribe_session_stream("missing").unwrap();
        outbound
            .record_pending_route(
                RawJsonRpcId::Number(3),
                ResponseRoute::Session("missing".to_string()),
                "session/load",
            )
            .await;
        let error = RawJsonRpcMessage::response(
            RawJsonRpcId::Number(3),
            Err(agent_client_protocol::Error::invalid_params()),
        );
        route(&outbound, &error).await;

        assert_eq!(method_from(&session_rx.recv().await.unwrap())["id"], 3);
        drop(session_rx);
        assert_eq!(session_count(&outbound), 0);
    }

    #[tokio::test]
    async fn closed_session_stream_is_removed() {
        let outbound = HttpOutbound::new();
        let mut connection_rx = outbound.connection_stream.try_acquire().unwrap();
        outbound.session_streams().confirm("open");
        outbound.session_streams().confirm("unwatched");
        let mut session_rx = outbound.subscribe_session_stream("open").unwrap();

        for (id, session_id) in [(4, "open"), (5, "unwatched")] {
            outbound
                .record_pending_route(
                    RawJsonRpcId::Number(id),
                    ResponseRoute::Session(session_id.to_string()),
                    "session/close",
                )
                .await;
            route(&outbound, &result(id, serde_json::json!({}))).await;
        }

        assert_eq!(session_count(&outbound), 0);
        // A subscribed stream still receives its close response. Without a
        // subscriber, the response goes to the connection stream.
        assert_eq!(method_from(&session_rx.recv().await.unwrap())["id"], 4);
        assert_eq!(method_from(&connection_rx.recv().await.unwrap())["id"], 5);
    }
}
