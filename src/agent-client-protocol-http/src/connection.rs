use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Weak},
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
    connection_stream: Arc<OutboundStream>,
    session_streams: RwLock<HashMap<String, Arc<OutboundStream>>>,
    pending_routes: Mutex<HashMap<RequestId, VecDeque<ResponseRoute>>>,
}

struct WebSocketOutbound {
    all_outbound: Arc<OutboundStream>,
}

struct OutboundStream {
    state: Mutex<OutboundStreamState>,
}

struct OutboundStreamState {
    replay: Option<VecDeque<String>>,
    subscribers: Vec<mpsc::Sender<String>>,
}

impl OutboundStream {
    fn new() -> Self {
        Self {
            state: Mutex::new(OutboundStreamState {
                replay: Some(VecDeque::new()),
                subscribers: Vec::new(),
            }),
        }
    }

    async fn push(&self, msg: String) {
        let mut state = self.state.lock().await;
        if let Some(replay) = state.replay.as_mut() {
            if replay.len() == OUTBOUND_STREAM_CAPACITY {
                replay.pop_front();
            }
            replay.push_back(msg);
        } else {
            state
                .subscribers
                .retain(|subscriber| match subscriber.try_send(msg.clone()) {
                    Ok(()) => true,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        debug!("outbound subscriber queue full; closing subscriber stream");
                        false
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => false,
                });
        }
    }

    async fn subscribe(&self) -> (Vec<String>, mpsc::Receiver<String>) {
        let mut state = self.state.lock().await;
        let (tx, receiver) = mpsc::channel(OUTBOUND_STREAM_CAPACITY);
        state.subscribers.push(tx);
        (
            state.replay.take().map(Vec::from).unwrap_or_default(),
            receiver,
        )
    }
}

pub(crate) const OUTBOUND_STREAM_CAPACITY: usize = 1024;

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

    pub(crate) async fn subscribe_connection_stream(
        &self,
    ) -> (Vec<String>, mpsc::Receiver<String>) {
        self.outbound_transport.subscribe_connection_stream().await
    }

    pub(crate) async fn subscribe_session_stream(
        &self,
        session_id: &str,
    ) -> (Vec<String>, mpsc::Receiver<String>) {
        self.outbound_transport
            .subscribe_session_stream(session_id)
            .await
    }

    pub(crate) async fn subscribe_all_outbound(&self) -> (Vec<String>, mpsc::Receiver<String>) {
        self.outbound_transport.subscribe_all_outbound().await
    }

    pub(crate) fn subscribe_closed(&self) -> watch::Receiver<bool> {
        self.closed_tx.subscribe()
    }

    #[cfg(test)]
    pub(crate) async fn push_connection_stream_for_test(&self, msg: String) {
        self.outbound_transport
            .push_connection_stream_for_test(msg)
            .await;
    }

    pub(crate) async fn start_router(self: &Arc<Self>) {
        let Some(mut rx) = self.outbound_rx.lock().await.take() else {
            return;
        };

        let connection = self.clone();
        *self.router_handle.lock().await = Some(tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                connection.route_outbound(msg).await;
            }
        }));
    }

    pub(crate) async fn route_outbound(&self, frame: TransportFrame) {
        self.outbound_transport.route_outbound(frame).await;
    }

    pub(crate) async fn recv_initial(&self) -> Option<TransportFrame> {
        let mut guard = self.outbound_rx.lock().await;
        let rx = guard.as_mut()?;
        rx.recv().await
    }

    pub(crate) async fn shutdown(&self) {
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

    async fn subscribe_connection_stream(&self) -> (Vec<String>, mpsc::Receiver<String>) {
        match self {
            Self::Http(http) => http.connection_stream.subscribe().await,
            Self::WebSocket(_) => empty_subscription(),
        }
    }

    async fn subscribe_session_stream(
        &self,
        session_id: &str,
    ) -> (Vec<String>, mpsc::Receiver<String>) {
        match self {
            Self::Http(http) => http.session_stream(session_id).await.subscribe().await,
            Self::WebSocket(_) => empty_subscription(),
        }
    }

    async fn subscribe_all_outbound(&self) -> (Vec<String>, mpsc::Receiver<String>) {
        match self {
            Self::Http(_) => empty_subscription(),
            Self::WebSocket(websocket) => websocket.all_outbound.subscribe().await,
        }
    }

    #[cfg(test)]
    async fn push_connection_stream_for_test(&self, msg: String) {
        let Self::Http(http) = self else {
            return;
        };

        http.connection_stream.push(msg).await;
    }

    async fn route_outbound(&self, frame: TransportFrame) {
        match frame {
            TransportFrame::Single(message) => {
                let serialized = match serde_json::to_string(&message) {
                    Ok(serialized) => serialized,
                    Err(error) => {
                        error!("failed to serialize outbound JSON-RPC message: {error}");
                        return;
                    }
                };
                match self {
                    Self::Http(http) => http.route_outbound(&message, serialized).await,
                    Self::WebSocket(websocket) => websocket.all_outbound.push(serialized).await,
                }
            }
            TransportFrame::Malformed { raw, .. } => match self {
                Self::Http(http) => http.connection_stream.push(raw).await,
                Self::WebSocket(websocket) => websocket.all_outbound.push(raw).await,
            },
            TransportFrame::Batch(batch) => {
                let serialized = match serde_json::to_string(&batch) {
                    Ok(serialized) => serialized,
                    Err(error) => {
                        error!("failed to serialize outbound JSON-RPC batch: {error}");
                        return;
                    }
                };
                match self {
                    Self::Http(http) => http.route_outbound_batch(&batch, serialized).await,
                    Self::WebSocket(websocket) => websocket.all_outbound.push(serialized).await,
                }
            }
        }
    }
}

impl HttpOutbound {
    fn new() -> Self {
        Self {
            connection_stream: Arc::new(OutboundStream::new()),
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

    async fn session_stream(&self, session_id: &str) -> Arc<OutboundStream> {
        if let Some(stream) = self.session_streams.read().await.get(session_id) {
            return stream.clone();
        }

        self.session_streams
            .write()
            .await
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(OutboundStream::new()))
            .clone()
    }

    async fn route_outbound(&self, msg: &RawJsonRpcMessage, serialized: String) {
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
                self.connection_stream.push(serialized).await;
            }
            ResponseRoute::Session(sid) => {
                trace!(target = %sid, "→ session-scoped stream");
                self.session_stream(&sid).await.push(serialized).await;
            }
        }
    }

    async fn route_outbound_batch(&self, batch: &TransportBatch, serialized: String) {
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
                self.connection_stream.push(serialized).await;
            }
            ResponseRoute::Session(session_id) => {
                trace!(target = %session_id, "→ session-scoped batch stream");
                self.session_stream(&session_id)
                    .await
                    .push(serialized)
                    .await;
            }
        }
    }
}

impl WebSocketOutbound {
    fn new() -> Self {
        Self {
            all_outbound: Arc::new(OutboundStream::new()),
        }
    }
}

fn empty_subscription() -> (Vec<String>, mpsc::Receiver<String>) {
    let (_tx, rx) = mpsc::channel(1);
    (Vec::new(), rx)
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
            debug!(connection_id = %conn_id_for_task, "HTTP ACP connection task ended");
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
        error!("HTTP outbound router task failed while draining: {error}");
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
        let (_replay, mut outbound) = connection.subscribe_connection_stream().await;

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
    async fn agent_exit_waits_for_router_to_flush_final_frame() {
        let emit = Arc::new(Notify::new());
        let registry = ConnectionRegistry::new(Arc::new(FinalFrameThenExitAgentFactory {
            emit: emit.clone(),
        }));
        let (connection_id, connection) = registry.create_connection().await;
        let (_replay, mut outbound) = connection.subscribe_connection_stream().await;
        connection.start_router().await;

        let stream = match &connection.outbound_transport {
            OutboundTransport::Http(http) => http.connection_stream.clone(),
            OutboundTransport::WebSocket(_) => unreachable!("created an HTTP connection"),
        };
        let state_guard = stream.state.lock().await;
        emit.notify_one();

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
        assert!(
            !*connection.subscribe_closed().borrow(),
            "stream closure must wait for the blocked outbound router"
        );

        drop(state_guard);
        let text = timeout(Duration::from_secs(1), outbound.recv())
            .await
            .unwrap()
            .expect("final frame should reach the established stream");
        let message = serde_json::from_str::<RawJsonRpcMessage>(&text).unwrap();
        assert!(matches!(
            message,
            RawJsonRpcMessage::Notification(notification)
                if notification.method.as_ref() == "test/final"
        ));

        timeout(Duration::from_secs(1), async {
            let mut closed = connection.subscribe_closed();
            while !*closed.borrow() {
                closed.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
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
        let (_connection_replay, mut connection_rx) =
            connection.subscribe_connection_stream().await;
        let (_session_replay, mut session_rx) =
            connection.subscribe_session_stream("session-1").await;

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
        let (_replay, mut connection_rx) = connection.subscribe_connection_stream().await;

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
        let (_connection_replay, mut connection_rx) = outbound.connection_stream.subscribe().await;
        let (_session_replay, mut session_rx) =
            outbound.session_stream("session-1").await.subscribe().await;

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
            .await;

        assert_eq!(
            timeout(Duration::from_secs(1), session_rx.recv())
                .await
                .unwrap(),
            Some(serialized)
        );
        assert!(connection_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn http_connection_does_not_retain_all_outbound_replay() {
        let exit = Arc::new(Notify::new());
        let message =
            RawJsonRpcMessage::notification("test/method".to_string(), serde_json::json!({}))
                .unwrap();
        let registry = ConnectionRegistry::new(Arc::new(SendThenWaitAgentFactory {
            message,
            exit: exit.clone(),
        }));
        let (_connection_id, connection) = registry.create_connection().await;
        let (_connection_replay, mut connection_rx) =
            connection.subscribe_connection_stream().await;

        connection.start_router().await;

        let text = timeout(Duration::from_secs(1), connection_rx.recv())
            .await
            .unwrap()
            .expect("message should reach HTTP connection stream");
        assert!(serde_json::from_str::<RawJsonRpcMessage>(&text).is_ok());

        let (all_replay, mut all_rx) = connection.subscribe_all_outbound().await;
        assert!(all_replay.is_empty());
        assert!(all_rx.try_recv().is_err());

        exit.notify_one();
        connection.shutdown().await;
    }

    #[tokio::test]
    async fn websocket_connection_does_not_retain_http_stream_replay() {
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
        let (_all_replay, mut all_rx) = connection.subscribe_all_outbound().await;

        connection.start_router().await;

        let text = timeout(Duration::from_secs(1), all_rx.recv())
            .await
            .unwrap()
            .expect("message should reach WebSocket all-outbound stream");
        assert!(serde_json::from_str::<RawJsonRpcMessage>(&text).is_ok());

        let (connection_replay, mut connection_rx) = connection.subscribe_connection_stream().await;
        let (session_replay, mut session_rx) =
            connection.subscribe_session_stream("session-1").await;
        assert!(connection_replay.is_empty());
        assert!(session_replay.is_empty());
        assert!(connection_rx.try_recv().is_err());
        assert!(session_rx.try_recv().is_err());

        exit.notify_one();
        connection.shutdown().await;
    }
}
