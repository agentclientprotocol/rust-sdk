//! HTTP-based MCP bridge transport.

use agent_client_protocol::{
    BoxFuture, Channel, ConnectTo, RawJsonRpcMessage, RawJsonRpcParams,
    role::mcp,
    schema::v1::{
        Notification as RpcNotification, Request as RpcRequest, RequestId, Response as RpcResponse,
    },
};
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response, Sse},
    routing::post,
};
use futures::{SinkExt, StreamExt as _, channel::mpsc, future::Either, stream::Stream};
use futures_concurrency::future::FutureExt as _;
use futures_concurrency::stream::StreamExt as _;
use rustc_hash::FxHashMap;
use std::{
    collections::{HashMap, VecDeque},
    pin::pin,
    sync::Arc,
};
use tokio::net::TcpListener;

use super::{BridgeConnection, BridgeMessage, actor::BridgeConnectionActor};

/// Runs an HTTP listener for MCP bridge connections.
pub async fn run_http_listener(
    tcp_listener: TcpListener,
    acp_id: String,
    mut bridge_tx: mpsc::Sender<BridgeMessage>,
) -> Result<(), agent_client_protocol::Error> {
    let (to_mcp_client_tx, to_mcp_client_rx) = mpsc::channel(128);

    bridge_tx
        .send(BridgeMessage::ConnectionReceived {
            acp_id,
            actor: BridgeConnectionActor::new(
                HttpMcpBridge::new(tcp_listener),
                bridge_tx.clone(),
                to_mcp_client_rx,
            ),
            connection: BridgeConnection::new(to_mcp_client_tx),
        })
        .await
        .map_err(|_| agent_client_protocol::Error::internal_error())?;

    Ok(())
}

/// A component that receives HTTP requests/responses using the HTTP transport
/// defined by the MCP protocol.
struct HttpMcpBridge {
    listener: tokio::net::TcpListener,
}

impl HttpMcpBridge {
    /// Creates a new HTTP-MCP bridge from an existing TCP listener.
    fn new(listener: tokio::net::TcpListener) -> Self {
        Self { listener }
    }
}

impl ConnectTo<mcp::Client> for HttpMcpBridge {
    async fn connect_to(
        self,
        client: impl ConnectTo<mcp::Server>,
    ) -> Result<(), agent_client_protocol::Error> {
        let (channel, serve_self) = self.into_channel_and_future();
        match futures::future::select(pin!(client.connect_to(channel)), serve_self).await {
            Either::Left((result, _)) | Either::Right((result, _)) => result,
        }
    }

    fn into_channel_and_future(
        self,
    ) -> (
        Channel,
        BoxFuture<'static, Result<(), agent_client_protocol::Error>>,
    )
    where
        Self: Sized,
    {
        let (channel_a, channel_b) = Channel::duplex();
        (channel_a, Box::pin(run(self.listener, channel_b)))
    }
}

/// Error type for responding to malformed HTTP requests.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
struct HttpError(#[from] agent_client_protocol::Error);

impl From<axum::Error> for HttpError {
    fn from(error: axum::Error) -> Self {
        HttpError(agent_client_protocol::util::internal_error(error))
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let message = format!("Error: {}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
    }
}

/// Run a webserver listening on `listener` for HTTP requests at `/`
/// and communicating those requests over `channel` to the JSON-RPC server.
async fn run(listener: TcpListener, channel: Channel) -> Result<(), agent_client_protocol::Error> {
    let (registration_tx, registration_rx) = mpsc::unbounded();
    let state = BridgeState { registration_tx };

    // The way that the MCP protocol works is a bit "special".
    //
    // Clients *POST* messages to `/`. Those are submitted to the MCP server.
    // If the message is a REQUEST, then the client waits until it gets a reply.
    // It expects the server to close the connection after responding.
    //
    // Clients can also issue a *GET* request. This will result in a stream of messages.
    //
    // Non-reply messages can be sent to any open stream (POST, GET, etc) but must be sent to
    // exactly one.
    //
    // There are provisions for "resuming" from a blocked point by tagging each message in the SSE
    // stream with an id, but we are not implementing that because I am lazy.
    async {
        let app = Router::new()
            .route("/", post(handle_post).get(handle_get))
            .with_state(Arc::new(state));

        axum::serve(listener, app)
            .await
            .map_err(agent_client_protocol::util::internal_error)
    }
    .race(RunningServer::new().run(channel, registration_rx))
    .await
}

/// The state we pass to our POST/GET handlers.
struct BridgeState {
    /// Where to send registration messages.
    registration_tx: mpsc::UnboundedSender<HttpMessage>,
}

/// Messages from HTTP handlers to the bridge server.
#[derive(Debug)]
#[allow(dead_code)]
enum HttpMessage {
    /// A JSON-RPC request (has an id, expects a response via the channel).
    Request {
        http_request_id: uuid::Uuid,
        request: RpcRequest<RawJsonRpcParams>,
        response_tx: mpsc::UnboundedSender<RawJsonRpcMessage>,
    },
    /// A JSON-RPC notification (no id, no response expected).
    Notification {
        http_request_id: uuid::Uuid,
        request: RpcNotification<RawJsonRpcParams>,
    },
    /// A JSON-RPC response from the client.
    Response {
        http_request_id: uuid::Uuid,
        response: RpcResponse<serde_json::Value>,
    },
    /// A GET request to open an SSE stream for server-initiated messages.
    Get {
        http_request_id: uuid::Uuid,
        response_tx: mpsc::UnboundedSender<RawJsonRpcMessage>,
    },
}

struct RunningServer {
    waiting_sessions: FxHashMap<RequestId, RegisteredSession>,
    general_sessions: Vec<RegisteredSession>,
    message_deque: VecDeque<RawJsonRpcMessage>,
}

impl RunningServer {
    fn new() -> Self {
        RunningServer {
            waiting_sessions: HashMap::default(),
            general_sessions: Vec::default(),
            message_deque: VecDeque::with_capacity(32),
        }
    }

    /// The main loop: listen for incoming HTTP messages and outgoing JSON-RPC messages.
    async fn run(
        mut self,
        mut channel: Channel,
        http_rx: mpsc::UnboundedReceiver<HttpMessage>,
    ) -> Result<(), agent_client_protocol::Error> {
        #[derive(Debug)]
        enum MultiplexMessage {
            FromHttpToChannel(HttpMessage),
            FromChannelToHttp(Result<RawJsonRpcMessage, agent_client_protocol::Error>),
        }

        let mut merged_stream = http_rx
            .map(MultiplexMessage::FromHttpToChannel)
            .merge(channel.rx.map(MultiplexMessage::FromChannelToHttp));

        while let Some(message) = merged_stream.next().await {
            tracing::trace!(?message, "received message");

            match message {
                MultiplexMessage::FromHttpToChannel(http_message) => {
                    self.handle_http_message(http_message, &mut channel.tx)?;
                }
                MultiplexMessage::FromChannelToHttp(message) => {
                    let message = message.unwrap_or_else(|err| {
                        RawJsonRpcMessage::response(RequestId::Null, Err(err))
                    });
                    self.message_deque.push_back(message);
                }
            }

            self.drain_jsonrpc_messages();
        }

        Ok(())
    }

    /// Handle an incoming HTTP message (request, notification, response, or GET).
    fn handle_http_message(
        &mut self,
        message: HttpMessage,
        channel_tx: &mut mpsc::UnboundedSender<
            Result<RawJsonRpcMessage, agent_client_protocol::Error>,
        >,
    ) -> Result<(), agent_client_protocol::Error> {
        match message {
            HttpMessage::Request {
                http_request_id,
                request,
                response_tx,
            } => {
                tracing::debug!(%http_request_id, ?request, "handling request");
                let request_id = request.id.clone();
                channel_tx
                    .unbounded_send(Ok(RawJsonRpcMessage::Request(request)))
                    .map_err(agent_client_protocol::util::internal_error)?;
                let session = RegisteredSession::new(response_tx);
                self.waiting_sessions.insert(request_id, session);
            }
            HttpMessage::Notification {
                http_request_id: _,
                request,
            } => {
                channel_tx
                    .unbounded_send(Ok(RawJsonRpcMessage::Notification(request)))
                    .map_err(agent_client_protocol::util::internal_error)?;
            }
            HttpMessage::Response {
                http_request_id: _,
                response,
            } => {
                channel_tx
                    .unbounded_send(Ok(RawJsonRpcMessage::Response(response)))
                    .map_err(agent_client_protocol::util::internal_error)?;
            }
            HttpMessage::Get {
                http_request_id: _,
                response_tx,
            } => {
                self.general_sessions
                    .push(RegisteredSession::new(response_tx));
            }
        }
        self.purge_closed_sessions();
        Ok(())
    }

    fn drain_jsonrpc_messages(&mut self) {
        while let Some(message) = self.message_deque.pop_front() {
            if let Some(message) = self.try_dispatch_jsonrpc_message(message) {
                self.message_deque.push_front(message);
                break;
            }
        }
    }

    fn try_dispatch_jsonrpc_message(
        &mut self,
        mut message: RawJsonRpcMessage,
    ) -> Option<RawJsonRpcMessage> {
        let message_id = message.response_id().cloned();

        if let Some(ref message_id) = message_id
            && let Some(session) = self.waiting_sessions.remove(message_id)
        {
            match session.outgoing_tx.unbounded_send(message) {
                Ok(()) => return None,
                Err(m) => {
                    assert!(m.is_disconnected());
                    message = m.into_inner();
                }
            }
        }

        self.purge_closed_sessions();
        let all_sessions = self
            .general_sessions
            .iter_mut()
            .chain(self.waiting_sessions.values_mut());
        for session in all_sessions {
            match session.outgoing_tx.unbounded_send(message) {
                Ok(()) => return None,
                Err(m) => {
                    assert!(m.is_disconnected());
                    message = m.into_inner();
                }
            }
        }

        Some(message)
    }

    fn purge_closed_sessions(&mut self) {
        self.general_sessions
            .retain(|session| !session.outgoing_tx.is_closed());
        self.waiting_sessions
            .retain(|_, session| !session.outgoing_tx.is_closed());
    }
}

struct RegisteredSession {
    #[allow(dead_code)]
    id: uuid::Uuid,
    outgoing_tx: mpsc::UnboundedSender<RawJsonRpcMessage>,
}

impl RegisteredSession {
    fn new(outgoing_tx: mpsc::UnboundedSender<RawJsonRpcMessage>) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            outgoing_tx,
        }
    }
}

/// Accept a POST request carrying a JSON-RPC message from an MCP client.
/// For requests (messages with id), we return an SSE stream.
/// For notifications/responses (messages without id), we return 202 Accepted.
async fn handle_post(
    State(state): State<Arc<BridgeState>>,
    body: String,
) -> Result<Response, HttpError> {
    let http_request_id = uuid::Uuid::new_v4();
    let message: RawJsonRpcMessage =
        serde_json::from_str(&body).map_err(agent_client_protocol::util::parse_error)?;

    match message {
        RawJsonRpcMessage::Request(request) => {
            let (tx, mut rx) = mpsc::unbounded();
            state
                .registration_tx
                .unbounded_send(HttpMessage::Request {
                    http_request_id,
                    request,
                    response_tx: tx,
                })
                .map_err(agent_client_protocol::util::internal_error)?;

            let stream = async_stream::stream! {
                while let Some(message) = rx.next().await {
                    match axum::response::sse::Event::default().json_data(message) {
                        Ok(v) => yield Ok(v),
                        Err(e) => yield Err(HttpError::from(e)),
                    }
                }
            };
            Ok(Sse::new(stream).into_response())
        }
        RawJsonRpcMessage::Notification(request) => {
            state
                .registration_tx
                .unbounded_send(HttpMessage::Notification {
                    http_request_id,
                    request,
                })
                .map_err(agent_client_protocol::util::internal_error)?;
            Ok(StatusCode::ACCEPTED.into_response())
        }
        RawJsonRpcMessage::Response(response) => {
            state
                .registration_tx
                .unbounded_send(HttpMessage::Response {
                    http_request_id,
                    response,
                })
                .map_err(agent_client_protocol::util::internal_error)?;
            Ok(StatusCode::ACCEPTED.into_response())
        }
    }
}

/// Accept a GET request from an MCP client.
/// Opens an SSE stream for server-initiated messages.
async fn handle_get(
    State(state): State<Arc<BridgeState>>,
) -> Result<Sse<impl Stream<Item = Result<axum::response::sse::Event, HttpError>>>, HttpError> {
    let http_request_id = uuid::Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded();
    state
        .registration_tx
        .unbounded_send(HttpMessage::Get {
            http_request_id,
            response_tx: tx,
        })
        .map_err(agent_client_protocol::util::internal_error)?;

    let stream = async_stream::stream! {
        while let Some(message) = rx.next().await {
            match axum::response::sse::Event::default().json_data(message) {
                Ok(v) => yield Ok(v),
                Err(e) => yield Err(HttpError::from(e)),
            }
        }
    };

    Ok(Sse::new(stream))
}
