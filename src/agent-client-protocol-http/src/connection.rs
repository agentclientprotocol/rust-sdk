use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Weak},
};

use agent_client_protocol::{Channel, RawJsonRpcMessage, schema::RequestId};
use futures::{SinkExt, StreamExt};
use tokio::sync::{Mutex, RwLock, mpsc, watch};
use tracing::{debug, error, trace};

use crate::protocol::session_id_from_message;

#[derive(Clone, Debug)]
pub(crate) enum ResponseRoute {
    Connection,
    Session(String),
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
    inbound_tx: mpsc::UnboundedSender<Result<RawJsonRpcMessage, agent_client_protocol::Error>>,
    outbound_rx: Mutex<Option<mpsc::UnboundedReceiver<RawJsonRpcMessage>>>,
    agent_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    router_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    closed_tx: watch::Sender<bool>,
    connection_stream: Arc<OutboundStream>,
    session_streams: RwLock<HashMap<String, Arc<OutboundStream>>>,
    all_outbound: Arc<OutboundStream>,
    pending_routes: Mutex<HashMap<RequestId, ResponseRoute>>,
}

impl Connection {
    pub(crate) fn send_to_agent(&self, msg: RawJsonRpcMessage) -> Result<(), &'static str> {
        self.inbound_tx
            .send(Ok(msg))
            .map_err(|_| "agent channel closed")
    }

    pub(crate) async fn record_pending_route(&self, id: RequestId, route: ResponseRoute) {
        if let Some(key) = pending_route_key(&id) {
            self.pending_routes.lock().await.insert(key, route);
        }
    }

    pub(crate) async fn ensure_session(&self, session_id: &str) {
        self.session_stream(session_id).await;
    }

    pub(crate) async fn subscribe_connection_stream(
        &self,
    ) -> (Vec<String>, mpsc::Receiver<String>) {
        self.connection_stream.subscribe().await
    }

    pub(crate) async fn subscribe_session_stream(
        &self,
        session_id: &str,
    ) -> (Vec<String>, mpsc::Receiver<String>) {
        self.session_stream(session_id).await.subscribe().await
    }

    pub(crate) async fn subscribe_all_outbound(&self) -> (Vec<String>, mpsc::Receiver<String>) {
        self.all_outbound.subscribe().await
    }

    pub(crate) fn subscribe_closed(&self) -> watch::Receiver<bool> {
        self.closed_tx.subscribe()
    }

    #[cfg(test)]
    pub(crate) async fn push_connection_stream_for_test(&self, msg: String) {
        self.connection_stream.push(msg).await;
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

    async fn route_outbound(&self, msg: RawJsonRpcMessage) {
        let serialized = match serde_json::to_string(&msg) {
            Ok(s) => s,
            Err(e) => {
                error!("failed to serialize outbound JSON-RPC message: {e}");
                return;
            }
        };

        self.all_outbound.push(serialized.clone()).await;

        let route = match &msg {
            RawJsonRpcMessage::Request(_) | RawJsonRpcMessage::Notification(_) => {
                session_id_from_message(&msg)
                    .map_or(ResponseRoute::Connection, ResponseRoute::Session)
            }
            RawJsonRpcMessage::Response(_) => {
                let route = match msg.response_id().and_then(pending_route_key) {
                    Some(key) => self.pending_routes.lock().await.remove(&key),
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

    pub(crate) async fn recv_initial(&self) -> Option<RawJsonRpcMessage> {
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
        let (channel, agent_future) = self.factory.spawn_agent();
        let (inbound_tx, mut inbound_rx) =
            mpsc::unbounded_channel::<Result<RawJsonRpcMessage, agent_client_protocol::Error>>();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<RawJsonRpcMessage>();
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
        let outbound = async move {
            while let Some(msg) = agent_rx.next().await {
                match msg {
                    Ok(m) => {
                        if outbound_tx.send(m).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        error!("agent emitted error: {e}");
                        break;
                    }
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
            connection_stream: Arc::new(OutboundStream::new()),
            session_streams: RwLock::new(HashMap::new()),
            all_outbound: Arc::new(OutboundStream::new()),
            pending_routes: Mutex::new(HashMap::new()),
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
    connection.close_streams();
    if let Some(h) = connection.router_handle.lock().await.take() {
        h.abort();
    }
}

fn pending_route_key(id: &RequestId) -> Option<RequestId> {
    match id {
        RequestId::Null => None,
        RequestId::Number(_) | RequestId::Str(_) => Some(id.clone()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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
                    .unbounded_send(Ok(RawJsonRpcMessage::response(
                        RequestId::Number(1),
                        Ok(serde_json::json!({ "done": true })),
                    )))
                    .unwrap();
                Ok(())
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
                agent.tx.unbounded_send(Ok(message)).unwrap();
                exit.notified().await;
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
    async fn agent_exit_drains_buffered_outbound_messages() {
        let registry = ConnectionRegistry::new(Arc::new(RespondThenExitAgentFactory));
        let (connection_id, connection) = registry.create_connection().await;

        let message = timeout(Duration::from_secs(1), connection.recv_initial())
            .await
            .unwrap()
            .expect("buffered response should be forwarded before teardown");

        assert!(matches!(
            message,
            RawJsonRpcMessage::Response(agent_client_protocol::schema::Response::Result {
                id: RequestId::Number(1),
                ..
            })
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
}
