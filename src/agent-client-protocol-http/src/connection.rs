use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use agent_client_protocol::{Channel, RawJsonRpcMessage, schema::RequestId};
use futures::{SinkExt, StreamExt};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tracing::{error, trace};

use crate::protocol::session_id_from_message;

#[derive(Clone, Debug)]
pub(crate) enum ResponseRoute {
    Connection,
    Session(String),
}

struct OutboundStream {
    tx: broadcast::Sender<String>,
    replay: Mutex<Option<VecDeque<String>>>,
}

impl OutboundStream {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            tx,
            replay: Mutex::new(Some(VecDeque::new())),
        }
    }

    async fn push(&self, msg: String) {
        let mut replay = self.replay.lock().await;
        if let Some(replay) = replay.as_mut() {
            if replay.len() == 1024 {
                replay.pop_front();
            }
            replay.push_back(msg);
        } else {
            drop(replay);
            drop(self.tx.send(msg));
        }
    }

    async fn subscribe(&self) -> (Vec<String>, broadcast::Receiver<String>) {
        let mut replay = self.replay.lock().await;
        let receiver = self.tx.subscribe();
        (replay.take().map(Vec::from).unwrap_or_default(), receiver)
    }
}

pub(crate) struct Connection {
    inbound_tx: mpsc::UnboundedSender<Result<RawJsonRpcMessage, agent_client_protocol::Error>>,
    outbound_rx: Mutex<Option<mpsc::UnboundedReceiver<RawJsonRpcMessage>>>,
    agent_handle: tokio::task::JoinHandle<()>,
    router_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
    ) -> (Vec<String>, broadcast::Receiver<String>) {
        self.connection_stream.subscribe().await
    }

    pub(crate) async fn subscribe_session_stream(
        &self,
        session_id: &str,
    ) -> (Vec<String>, broadcast::Receiver<String>) {
        self.session_stream(session_id).await.subscribe().await
    }

    pub(crate) async fn subscribe_all_outbound(
        &self,
    ) -> (Vec<String>, broadcast::Receiver<String>) {
        self.all_outbound.subscribe().await
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

    pub(crate) async fn recv_initial(&self) -> Option<String> {
        let mut guard = self.outbound_rx.lock().await;
        let rx = guard.as_mut()?;
        serde_json::to_string(&rx.recv().await?).ok()
    }

    pub(crate) async fn shutdown(&self) {
        self.agent_handle.abort();
        if let Some(h) = self.router_handle.lock().await.take() {
            h.abort();
        }
    }
}

pub(crate) struct ConnectionRegistry {
    factory: Arc<dyn AgentFactory>,
    connections: RwLock<HashMap<String, Arc<Connection>>>,
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
            connections: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) async fn create_connection(&self) -> (String, Arc<Connection>) {
        let (mut channel, agent_future) = self.factory.spawn_agent();
        let (inbound_tx, mut inbound_rx) =
            mpsc::unbounded_channel::<Result<RawJsonRpcMessage, agent_client_protocol::Error>>();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<RawJsonRpcMessage>();

        let pump = async move {
            let inbound = async {
                while let Some(msg) = inbound_rx.recv().await {
                    if channel.tx.send(msg).await.is_err() {
                        break;
                    }
                }
                drop(channel.tx.close().await);
            };
            let outbound = async {
                while let Some(msg) = channel.rx.next().await {
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
            futures::join!(inbound, outbound);
        };

        let connection_id = uuid::Uuid::new_v4().to_string();
        let conn_id_for_task = connection_id.clone();
        let agent_handle = tokio::spawn(async move {
            let agent = async move {
                if let Err(e) = agent_future.await {
                    error!(connection_id = %conn_id_for_task, "ACP agent task error: {e}");
                }
            };
            futures::pin_mut!(agent);
            futures::pin_mut!(pump);
            futures::future::select(agent, pump).await;
        });

        let connection = Arc::new(Connection {
            inbound_tx,
            outbound_rx: Mutex::new(Some(outbound_rx)),
            agent_handle,
            router_handle: Mutex::new(None),
            connection_stream: Arc::new(OutboundStream::new()),
            session_streams: RwLock::new(HashMap::new()),
            all_outbound: Arc::new(OutboundStream::new()),
            pending_routes: Mutex::new(HashMap::new()),
        });

        self.connections
            .write()
            .await
            .insert(connection_id.clone(), connection.clone());

        (connection_id, connection)
    }

    pub(crate) async fn get(&self, connection_id: &str) -> Option<Arc<Connection>> {
        self.connections.read().await.get(connection_id).cloned()
    }

    pub(crate) async fn remove(&self, connection_id: &str) -> Option<Arc<Connection>> {
        self.connections.write().await.remove(connection_id)
    }
}

fn pending_route_key(id: &RequestId) -> Option<RequestId> {
    match id {
        RequestId::Null => None,
        RequestId::Number(_) | RequestId::Str(_) => Some(id.clone()),
    }
}
