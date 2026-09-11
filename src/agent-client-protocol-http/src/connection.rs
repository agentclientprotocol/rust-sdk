use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex as StdMutex, Weak},
};

use agent_client_protocol::{
    Channel, RawJsonRpcMessage, TransportBatch, TransportBatchEntry, TransportFrame,
    schema::v1::RequestId,
};
use futures::{SinkExt, StreamExt};
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

struct HttpOutbound {
    connection_stream: OutboundMailbox,
    session_streams: RwLock<HashMap<String, Arc<OutboundMailbox>>>,
    pending_routes: Mutex<HashMap<RequestId, VecDeque<ResponseRoute>>>,
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
        })
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

    pub(crate) async fn record_pending_route(&self, id: RequestId, route: ResponseRoute) {
        self.outbound_transport
            .record_pending_route(id, route)
            .await;
    }

    pub(crate) async fn ensure_session(&self, session_id: &str) {
        self.outbound_transport.ensure_session(session_id).await;
    }

    pub(crate) fn subscribe_connection_stream(&self) -> Option<OutboundLease> {
        self.outbound_transport.subscribe_connection_stream()
    }

    pub(crate) async fn subscribe_session_stream(&self, session_id: &str) -> Option<OutboundLease> {
        self.outbound_transport
            .subscribe_session_stream(session_id)
            .await
    }

    pub(crate) fn subscribe_all_outbound(&self) -> Option<OutboundLease> {
        self.outbound_transport.subscribe_all_outbound()
    }

    pub(crate) fn enqueue_websocket_text(&self, text: String) -> Result<(), &'static str> {
        self.outbound_transport.enqueue_websocket_text(text)
    }

    pub(crate) fn subscribe_closed(&self) -> watch::Receiver<bool> {
        self.closed_tx.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn push_connection_stream_for_test(&self, msg: String) -> Result<(), &'static str> {
        self.outbound_transport.push_connection_stream_for_test(msg)
    }

    #[cfg(test)]
    pub(crate) fn push_all_outbound_for_test(&self, msg: String) -> Result<(), &'static str> {
        self.enqueue_websocket_text(msg)
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
        // awaits the router in `close_connection_task` before closing streams.
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

    async fn record_pending_route(&self, id: RequestId, route: ResponseRoute) {
        let Self::Http(http) = self else {
            return;
        };

        http.record_pending_route(id, route).await;
    }

    async fn ensure_session(&self, session_id: &str) {
        let Self::Http(http) = self else {
            return;
        };

        http.ensure_session(session_id).await;
    }

    fn subscribe_connection_stream(&self) -> Option<OutboundLease> {
        match self {
            Self::Http(http) => http.connection_stream.try_acquire(),
            Self::WebSocket(_) => None,
        }
    }

    async fn subscribe_session_stream(&self, session_id: &str) -> Option<OutboundLease> {
        match self {
            Self::Http(http) => http.session_stream(session_id).await.try_acquire(),
            Self::WebSocket(_) => None,
        }
    }

    fn subscribe_all_outbound(&self) -> Option<OutboundLease> {
        match self {
            Self::Http(_) => None,
            Self::WebSocket(websocket) => websocket.all_outbound.try_acquire(),
        }
    }

    fn enqueue_websocket_text(&self, text: String) -> Result<(), &'static str> {
        let Self::WebSocket(websocket) = self else {
            return Err("not a WebSocket connection");
        };
        websocket.all_outbound.push(text)
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
                let serialized = match serde_json::to_string(&message) {
                    Ok(serialized) => serialized,
                    Err(error) => {
                        error!("failed to serialize outbound JSON-RPC message: {error}");
                        return Err("failed to serialize outbound JSON-RPC message");
                    }
                };
                match self {
                    Self::Http(http) => http.route_outbound(&message, serialized).await,
                    Self::WebSocket(websocket) => websocket.all_outbound.push(serialized),
                }
            }
            TransportFrame::Malformed { raw, .. } => match self {
                Self::Http(http) => http.connection_stream.push(raw),
                Self::WebSocket(websocket) => websocket.all_outbound.push(raw),
            },
            TransportFrame::Batch(batch) => {
                let serialized = match serde_json::to_string(&batch) {
                    Ok(serialized) => serialized,
                    Err(error) => {
                        error!("failed to serialize outbound JSON-RPC batch: {error}");
                        return Err("failed to serialize outbound JSON-RPC batch");
                    }
                };
                match self {
                    Self::Http(http) => http.route_outbound_batch(&batch, serialized).await,
                    Self::WebSocket(websocket) => websocket.all_outbound.push(serialized),
                }
            }
        }
    }
}

impl HttpOutbound {
    fn new() -> Self {
        Self {
            connection_stream: OutboundMailbox::new(),
            session_streams: RwLock::new(HashMap::new()),
            pending_routes: Mutex::new(HashMap::new()),
        }
    }

    async fn record_pending_route(&self, id: RequestId, route: ResponseRoute) {
        if let Some(key) = pending_route_key(&id) {
            self.pending_routes
                .lock()
                .await
                .entry(key)
                .or_default()
                .push_back(route);
        }
    }

    async fn ensure_session(&self, session_id: &str) {
        self.session_stream(session_id).await;
    }

    async fn session_stream(&self, session_id: &str) -> Arc<OutboundMailbox> {
        if let Some(stream) = self.session_streams.read().await.get(session_id) {
            return stream.clone();
        }

        self.session_streams
            .write()
            .await
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(OutboundMailbox::new()))
            .clone()
    }

    async fn route_outbound(
        &self,
        msg: &RawJsonRpcMessage,
        serialized: String,
    ) -> Result<(), &'static str> {
        let route = match msg {
            RawJsonRpcMessage::Request(_) | RawJsonRpcMessage::Notification(_) => {
                session_id_from_message(msg)
                    .map_or(ResponseRoute::Connection, ResponseRoute::Session)
            }
            RawJsonRpcMessage::Response(_) => {
                let route = match msg.response_id().and_then(pending_route_key) {
                    Some(key) => {
                        let mut pending_routes = self.pending_routes.lock().await;
                        take_pending_route(&mut pending_routes, &key)
                    }
                    None => None,
                };
                route.unwrap_or(ResponseRoute::Connection)
            }
        };

        match route {
            ResponseRoute::Connection => {
                trace!(target = "connection", "→ connection-scoped stream");
                self.connection_stream.push(serialized)
            }
            ResponseRoute::Session(sid) => {
                trace!(target = %sid, "→ session-scoped stream");
                self.session_stream(&sid).await.push(serialized)
            }
        }
    }

    async fn route_outbound_batch(
        &self,
        batch: &TransportBatch,
        serialized: String,
    ) -> Result<(), &'static str> {
        let mut pending_routes = self.pending_routes.lock().await;
        let mut common_route = None;
        let mut routes_disagree = false;
        for entry in batch.entries() {
            let route = match entry {
                TransportBatchEntry::Message(message) => message
                    .response_id()
                    .and_then(pending_route_key)
                    .and_then(|key| take_pending_route(&mut pending_routes, &key))
                    .unwrap_or(ResponseRoute::Connection),
                TransportBatchEntry::Malformed { .. } => ResponseRoute::Connection,
            };
            match &common_route {
                None => common_route = Some(route),
                Some(common_route) if common_route == &route => {}
                Some(_) => routes_disagree = true,
            }
        }
        drop(pending_routes);

        let route = if routes_disagree {
            ResponseRoute::Connection
        } else {
            common_route.unwrap_or(ResponseRoute::Connection)
        };
        match route {
            ResponseRoute::Connection => {
                trace!(target = "connection", "→ connection-scoped batch stream");
                self.connection_stream.push(serialized)
            }
            ResponseRoute::Session(session_id) => {
                trace!(target = %session_id, "→ session-scoped batch stream");
                self.session_stream(&session_id).await.push(serialized)
            }
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
    fn spawn_agent(
        &self,
    ) -> (
        Channel,
        futures::future::BoxFuture<'static, agent_client_protocol::Result<()>>,
    );
}

impl<F, C> AgentFactory for F
where
    F: Fn() -> C + Send + Sync + 'static,
    C: agent_client_protocol::ConnectTo<agent_client_protocol::Client>,
{
    fn spawn_agent(
        &self,
    ) -> (
        Channel,
        futures::future::BoxFuture<'static, agent_client_protocol::Result<()>>,
    ) {
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
        let outbound = async move {
            while let Some(msg) = agent_rx.next().await {
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
            let agent = async move {
                if let Err(e) = agent_future.await {
                    error!(connection_id = %conn_id_for_agent, "ACP agent task error: {e}");
                }
            };
            futures::pin_mut!(agent);
            futures::pin_mut!(pump);
            match futures::future::select(agent, pump).await {
                futures::future::Either::Left(((), pump)) => {
                    inbound_abort.abort();
                    pump.await;
                }
                futures::future::Either::Right(((), _agent)) => {}
            }
            debug!(connection_id = %conn_id_for_task, "ACP connection task ended");
            connections.write().await.remove(&conn_id_for_task);
            close_connection_task(connection_for_task).await;
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

async fn close_connection_task(connection: Weak<Connection>) {
    let Some(connection) = connection.upgrade() else {
        return;
    };
    let router_handle = connection.router_handle.lock().await.take();
    if let Some(h) = router_handle
        && let Err(error) = h.await
    {
        error!("outbound router task failed while draining: {error}");
    }
    connection.close_streams();
}

fn pending_route_key(id: &RequestId) -> Option<RequestId> {
    match id {
        RequestId::Null => None,
        RequestId::Number(_) | RequestId::Str(_) => Some(id.clone()),
    }
}

fn take_pending_route(
    pending_routes: &mut HashMap<RequestId, VecDeque<ResponseRoute>>,
    key: &RequestId,
) -> Option<ResponseRoute> {
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

    use agent_client_protocol::TransportBatch;
    use futures::future::BoxFuture;
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
        let mut slow_session = outbound
            .session_stream("slow-session")
            .await
            .try_acquire()
            .unwrap();
        let mut fast_session = outbound
            .session_stream("fast-session")
            .await
            .try_acquire()
            .unwrap();

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
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (agent, transport) = Channel::duplex();
            let exit = self.exit.clone();
            let future = Box::pin(async move {
                exit.notified().await;
                drop(agent);
                Ok(())
            });

            (transport, future)
        }
    }

    struct RespondThenExitAgentFactory;

    impl AgentFactory for RespondThenExitAgentFactory {
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (agent, transport) = Channel::duplex();
            let future = Box::pin(async move {
                agent
                    .tx
                    .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::response(
                        RequestId::Number(1),
                        Ok(serde_json::json!({ "done": true })),
                    )))
                    .unwrap();
                Ok(())
            });

            (transport, future)
        }
    }

    struct MalformedThenWaitAgentFactory {
        emit: Arc<Notify>,
    }

    impl AgentFactory for MalformedThenWaitAgentFactory {
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (agent, transport) = Channel::duplex();
            let emit = self.emit.clone();
            let future = Box::pin(async move {
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

            (transport, future)
        }
    }

    struct SendThenWaitAgentFactory {
        message: RawJsonRpcMessage,
        exit: Arc<Notify>,
    }

    impl AgentFactory for SendThenWaitAgentFactory {
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (agent, transport) = Channel::duplex();
            let message = self.message.clone();
            let exit = self.exit.clone();
            let future = Box::pin(async move {
                agent
                    .tx
                    .unbounded_send(TransportFrame::Single(message))
                    .unwrap();
                exit.notified().await;
                Ok(())
            });

            (transport, future)
        }
    }

    struct BatchThenWaitAgentFactory {
        exit: Arc<Notify>,
    }

    impl AgentFactory for BatchThenWaitAgentFactory {
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (agent, transport) = Channel::duplex();
            let exit = self.exit.clone();
            let future = Box::pin(async move {
                let batch = TransportBatch::from_messages([
                    RawJsonRpcMessage::notification(
                        "test/first".to_string(),
                        serde_json::json!({}),
                    )
                    .unwrap(),
                    RawJsonRpcMessage::notification(
                        "test/second".to_string(),
                        serde_json::json!({}),
                    )
                    .unwrap(),
                ])
                .expect("test batch is non-empty");
                agent
                    .tx
                    .unbounded_send(TransportFrame::Batch(batch))
                    .unwrap();
                exit.notified().await;
                Ok(())
            });

            (transport, future)
        }
    }

    struct FinalFrameThenExitAgentFactory {
        emit: Arc<Notify>,
    }

    impl AgentFactory for FinalFrameThenExitAgentFactory {
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (agent, transport) = Channel::duplex();
            let emit = self.emit.clone();
            let future = Box::pin(async move {
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

            (transport, future)
        }
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
                agent_client_protocol::schema::v1::Response::Result {
                    id: RequestId::Number(1),
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
        let registry = ConnectionRegistry::new(Arc::new(FinalFrameThenExitAgentFactory {
            emit: emit.clone(),
        }));
        let (connection_id, connection) = registry.create_connection().await;
        let mut outbound = connection.subscribe_connection_stream().unwrap();
        connection.start_router().await;

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
        let mut session_rx = connection
            .subscribe_session_stream("session-1")
            .await
            .unwrap();

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
    async fn batch_is_relayed_as_one_connection_stream_frame() {
        let exit = Arc::new(Notify::new());
        let registry =
            ConnectionRegistry::new(Arc::new(BatchThenWaitAgentFactory { exit: exit.clone() }));
        let (_connection_id, connection) = registry.create_connection().await;
        let mut connection_rx = connection.subscribe_connection_stream().unwrap();

        connection.start_router().await;

        let text = timeout(Duration::from_secs(1), connection_rx.recv())
            .await
            .unwrap()
            .expect("batch should reach the connection stream");
        let batch = serde_json::from_str::<serde_json::Value>(&text).unwrap();
        let entries = batch.as_array().expect("batch should remain an array");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["method"], "test/first");
        assert_eq!(entries[1]["method"], "test/second");
        assert!(connection_rx.try_recv().is_err());

        exit.notify_one();
        connection.shutdown().await;
    }

    #[tokio::test]
    async fn duplicate_batch_response_ids_consume_each_pending_route() {
        let outbound = HttpOutbound::new();
        let mut connection_rx = outbound.connection_stream.try_acquire().unwrap();
        let mut session_rx = outbound
            .session_stream("session-1")
            .await
            .try_acquire()
            .unwrap();

        let id = RequestId::Number(21);
        let route = ResponseRoute::Session("session-1".to_string());
        outbound
            .record_pending_route(id.clone(), route.clone())
            .await;
        outbound.record_pending_route(id.clone(), route).await;

        let batch = TransportBatch::from_messages([
            RawJsonRpcMessage::response(id.clone(), Ok(serde_json::json!({ "slot": 1 }))),
            RawJsonRpcMessage::response(id, Ok(serde_json::json!({ "slot": 2 }))),
        ])
        .expect("duplicate-ID response batch is non-empty");
        let serialized = serde_json::to_string(&batch).unwrap();

        outbound
            .route_outbound_batch(&batch, serialized.clone())
            .await
            .unwrap();

        assert_eq!(
            timeout(Duration::from_secs(1), session_rx.recv())
                .await
                .unwrap(),
            Some(serialized)
        );
        assert!(connection_rx.try_recv().is_err());
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
        assert!(
            connection
                .subscribe_session_stream("session-1")
                .await
                .is_none()
        );

        exit.notify_one();
        connection.shutdown().await;
    }
}
