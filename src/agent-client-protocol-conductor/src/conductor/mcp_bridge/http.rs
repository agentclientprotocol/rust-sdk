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
use agent_client_protocol_core::{BoxFuture, Channel, ConnectTo, jsonrpcmsg::Message, role::mcp};
use std::{collections::VecDeque, pin::pin, sync::Arc};
use tokio::net::TcpListener;

use crate::conductor::{
    ConductorMessage,
    mcp_bridge::{McpBridgeConnection, McpBridgeConnectionActor},
};

/// Runs an HTTP listener for MCP bridge connections
pub async fn run_http_listener(
    tcp_listener: TcpListener,
    acp_url: String,
    mut conductor_tx: mpsc::Sender<ConductorMessage>,
) -> Result<(), agent_client_protocol_core::Error> {
    let (to_mcp_client_tx, to_mcp_client_rx) = mpsc::channel(128);

    // When we send this message to the conductor,
    // it is going to go through a step or two and eventually
    // spawn the McpBridgeConnectionActor, which will ferry MCP requests
    // back and forth.
    conductor_tx
        .send(ConductorMessage::McpConnectionReceived {
            acp_url: acp_url,
            actor: McpBridgeConnectionActor::new(
                HttpMcpBridge::new(tcp_listener),
                conductor_tx.clone(),
                to_mcp_client_rx,
            ),
            connection: McpBridgeConnection::new(to_mcp_client_tx),
        })
        .await
        .map_err(|_| agent_client_protocol_core::Error::internal_error())?;

    Ok(())
}

/// A component that receives HTTP requests/responses using the HTTP transport defined by the MCP protocol.
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
    async fn connect_to(self, client: impl ConnectTo<mcp::Server>) -> Result<(), agent_client_protocol_core::Error> {
        let (channel, serve_self) = self.into_channel_and_future();
        match futures::future::select(pin!(client.connect_to(channel)), serve_self).await {
            Either::Left((result, _)) => result,
            Either::Right((result, _)) => result,
        }
    }

    fn into_channel_and_future(self) -> (Channel, BoxFuture<'static, Result<(), agent_client_protocol_core::Error>>)
    where
        Self: Sized,
    {
        let (channel_a, channel_b) = Channel::duplex();
        (channel_a, Box::pin(run(self.listener, channel_b)))
    }
}

/// Error type that we use to respond to malformed HTTP requests.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
struct HttpError(#[from] agent_client_protocol_core::Error);

impl From<axum::Error> for HttpError {
    fn from(error: axum::Error) -> Self {
        HttpError(agent_client_protocol_core::util::internal_error(error))
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
async fn run(listener: TcpListener, channel: Channel) -> Result<(), agent_client_protocol_core::Error> {
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
    // Non-reply messages can be send to any open stream (POST, GET, etc) but must be sent to
    // exactly one.
    //
    // There are provisions for "resuming" from a blocked point by tagging each message in the SSE stream
    // with an id, but we are not implementing that because I am lazy.
    async {
        let app = Router::new()
            .route("/", post(handle_post).get(handle_get))
            .with_state(Arc::new(state));

        axum::serve(listener, app)
            .await
            .map_err(agent_client_protocol_core::util::internal_error)
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
enum HttpMessage {
    /// A JSON-RPC request (has an id, expects a response via the channel)
    Request {
        http_request_id: uuid::Uuid,
        request: agent_client_protocol_core::jsonrpcmsg::Request,
        response_tx: mpsc::UnboundedSender<agent_client_protocol_core::jsonrpcmsg::Message>,
    },
    /// A JSON-RPC notification (no id, no response expected)
    Notification {
        http_request_id: uuid::Uuid,
        request: agent_client_protocol_core::jsonrpcmsg::Request,
    },
    /// A JSON-RPC response from the client
    Response {
        http_request_id: uuid::Uuid,
        response: agent_client_protocol_core::jsonrpcmsg::Response,
    },
    /// A GET request to open an SSE stream for server-initiated messages
    Get {
        http_request_id: uuid::Uuid,
        response_tx: mpsc::UnboundedSender<agent_client_protocol_core::jsonrpcmsg::Message>,
    },
}

/// Clone of `agent_client_protocol_core::jsonrpcmsg::Id` since for unfathomable reasons that does not impl Hash
#[derive(Eq, PartialEq, PartialOrd, Ord, Hash, Debug, Clone)]
enum JsonRpcId {
    /// String identifier
    String(String),
    /// Numeric identifier
    Number(u64),
    /// Null identifier (for notifications)
    Null,
}

impl From<agent_client_protocol_core::jsonrpcmsg::Id> for JsonRpcId {
    fn from(id: agent_client_protocol_core::jsonrpcmsg::Id) -> Self {
        match id {
            agent_client_protocol_core::jsonrpcmsg::Id::String(s) => JsonRpcId::String(s),
            agent_client_protocol_core::jsonrpcmsg::Id::Number(n) => JsonRpcId::Number(n),
            agent_client_protocol_core::jsonrpcmsg::Id::Null => JsonRpcId::Null,
        }
    }
}

struct RunningServer {
    waiting_sessions: FxHashMap<JsonRpcId, RegisteredSession>,
    general_sessions: Vec<RegisteredSession>,
    message_deque: VecDeque<agent_client_protocol_core::jsonrpcmsg::Message>,
}

impl RunningServer {
    fn new() -> Self {
        RunningServer {
            waiting_sessions: Default::default(),
            general_sessions: Default::default(),
            message_deque: VecDeque::with_capacity(32),
        }
    }

    /// The main loop: listen for incoming HTTP messages and outgoing JSON-RPC messages.
    ///
    /// # Parameters
    ///
    /// * `channel`: The channel to use for sending/receiving JSON-RPC messages.
    /// * `http_rx`: The receiver for messages from HTTP handlers.
    async fn run(
        mut self,
        mut channel: Channel,
        http_rx: mpsc::UnboundedReceiver<HttpMessage>,
    ) -> Result<(), agent_client_protocol_core::Error> {
        #[derive(Debug)]
        enum MultiplexMessage {
            FromHttpToChannel(HttpMessage),
            FromChannelToHttp(Result<agent_client_protocol_core::jsonrpcmsg::Message, agent_client_protocol_core::Error>),
        }

        let mut merged_stream = http_rx
            .map(MultiplexMessage::FromHttpToChannel)
            .merge(channel.rx.map(MultiplexMessage::FromChannelToHttp));

        while let Some(message) = merged_stream.next().await {
            tracing::trace!(?message, "received message");

            match message {
                MultiplexMessage::FromHttpToChannel(http_message) => {
                    self.handle_http_message(http_message, &mut channel.tx)
                        .await?
                }

                MultiplexMessage::FromChannelToHttp(message) => {
                    let message = message.unwrap_or_else(|err| {
                        agent_client_protocol_core::jsonrpcmsg::Message::Response(agent_client_protocol_core::jsonrpcmsg::Response::error(
                            agent_client_protocol_core::util::into_jsonrpc_error(err),
                            None,
                        ))
                    });
                    tracing::debug!(
                        queue_len = self.message_deque.len() + 1,
                        ?message,
                        "enqueuing outgoing message"
                    );
                    self.message_deque.push_back(message);
                }
            }

            self.drain_jsonrpc_messages().await?;
        }

        tracing::trace!("http connection terminating");

        Ok(())
    }

    /// Handle an incoming HTTP message (request, notification, response, or GET).
    async fn handle_http_message(
        &mut self,
        message: HttpMessage,
        channel_tx: &mut mpsc::UnboundedSender<Result<agent_client_protocol_core::jsonrpcmsg::Message, agent_client_protocol_core::Error>>,
    ) -> Result<(), agent_client_protocol_core::Error> {
        match message {
            HttpMessage::Request {
                http_request_id,
                request,
                response_tx,
            } => {
                tracing::debug!(%http_request_id, ?request, "handling request");
                let request_id = request.id.clone().map(JsonRpcId::from);

                // Send to the JSON-RPC server
                channel_tx
                    .unbounded_send(Ok(Message::Request(request)))
                    .map_err(agent_client_protocol_core::util::internal_error)?;

                // Register to receive the response
                let session = RegisteredSession::new(response_tx);
                if let Some(id) = request_id {
                    tracing::debug!(%http_request_id, session_id = %session.id, ?id, "registering waiting session");
                    self.waiting_sessions.insert(id, session);
                } else {
                    // Request without id - treat like a general session
                    tracing::debug!(%http_request_id, session_id = %session.id, "registering general session (request without id)");
                    self.general_sessions.push(session);
                }
            }

            HttpMessage::Notification {
                http_request_id,
                request,
            } => {
                tracing::debug!(%http_request_id, ?request, "handling notification");
                // Just forward to the server, no response tracking needed
                channel_tx
                    .unbounded_send(Ok(Message::Request(request)))
                    .map_err(agent_client_protocol_core::util::internal_error)?;
            }

            HttpMessage::Response {
                http_request_id,
                response,
            } => {
                tracing::debug!(%http_request_id, ?response, "handling response");
                // Forward to the server
                channel_tx
                    .unbounded_send(Ok(Message::Response(response)))
                    .map_err(agent_client_protocol_core::util::internal_error)?;
            }

            HttpMessage::Get {
                http_request_id,
                response_tx,
            } => {
                let session = RegisteredSession::new(response_tx);
                tracing::debug!(
                    %http_request_id,
                    session_id = %session.id,
                    queued_messages = self.message_deque.len(),
                    "handling GET (opening SSE stream)"
                );
                // Register as a general session to receive server-initiated messages
                self.general_sessions.push(session);
            }
        }

        // Purge closed sessions for good hygiene
        self.purge_closed_sessions();

        Ok(())
    }

    /// Remove messages from the queue and send them.
    /// Stop if we cannot find places to send them.
    async fn drain_jsonrpc_messages(&mut self) -> Result<(), agent_client_protocol_core::Error> {
        if !self.message_deque.is_empty() {
            tracing::debug!(
                queue_len = self.message_deque.len(),
                general_sessions = self.general_sessions.len(),
                waiting_sessions = self.waiting_sessions.len(),
                "draining message queue"
            );
        }

        while let Some(message) = self.message_deque.pop_front() {
            match self.try_dispatch_jsonrpc_message(message).await? {
                None => {
                    tracing::debug!(
                        remaining = self.message_deque.len(),
                        "message dispatched successfully"
                    );
                }

                Some(message) => {
                    tracing::debug!(
                        remaining = self.message_deque.len() + 1,
                        "no available session, re-enqueuing message"
                    );
                    self.message_deque.push_front(message);
                    break;
                }
            }
        }

        Ok(())
    }

    /// Invoked when there is an outgoing JSON-RPC message to send.
    /// Tries to find a suitable place to send it.
    /// If it succeeds, returns `Ok(None)`.
    /// If there is no place to send it, returns `Ok(Some(message))`.
    async fn try_dispatch_jsonrpc_message(
        &mut self,
        mut message: agent_client_protocol_core::jsonrpcmsg::Message,
    ) -> Result<Option<agent_client_protocol_core::jsonrpcmsg::Message>, agent_client_protocol_core::Error> {
        // Extract the id of the message we are replying to, if any
        let message_id = match &message {
            Message::Response(response) => match &response.id {
                Some(v) => Some(v.clone().into()),
                None => None,
            },
            Message::Request(_) => None,
        };

        tracing::debug!(?message_id, "attempting to dispatch JSON-RPC message");

        // If there is a specific id, try to send the message to that sender.
        // This also removes them from the list of waiting sessions.
        if let Some(ref message_id) = message_id {
            if let Some(session) = self.waiting_sessions.remove(message_id) {
                tracing::debug!(session_id = %session.id, "found waiting session, attempting send");

                match session.outgoing_tx.unbounded_send(message) {
                    // Successfully sent the message, return
                    Ok(()) => {
                        tracing::debug!(session_id = %session.id, "sent to waiting session");
                        return Ok(None);
                    }

                    // If the sender died, just recover the message and send it to anyone.
                    Err(m) => {
                        tracing::debug!(session_id = %session.id, "waiting session disconnected");
                        // If that sender is dead, remove them from the list
                        // and recover the message.
                        assert!(m.is_disconnected());
                        message = m.into_inner();
                    }
                }
            }
        }

        // Try to find *somewhere* to send the message
        self.purge_closed_sessions();
        tracing::debug!(
            general_sessions = self.general_sessions.len(),
            waiting_sessions = self.waiting_sessions.len(),
            "trying to find any active session"
        );
        let all_sessions = self
            .general_sessions
            .iter_mut()
            .chain(self.waiting_sessions.values_mut());
        for session in all_sessions {
            tracing::trace!(session_id = %session.id, "trying session");
            match session.outgoing_tx.unbounded_send(message) {
                Ok(()) => {
                    tracing::debug!(session_id = %session.id, "sent to session");
                    return Ok(None);
                }

                Err(m) => {
                    tracing::debug!(session_id = %session.id, "session disconnected, trying next");
                    assert!(m.is_disconnected());
                    message = m.into_inner();
                }
            }
        }

        // If we don't find anywhere to send the message, return it.
        Ok(Some(message))
    }

    /// Purge sessions from the bridge state where the receiver is closed.
    /// This happens when the HTTP client disconnects.
    fn purge_closed_sessions(&mut self) {
        self.general_sessions
            .retain(|session| !session.outgoing_tx.is_closed());
        self.waiting_sessions
            .retain(|_, session| !session.outgoing_tx.is_closed());
    }
}

struct RegisteredSession {
    id: uuid::Uuid,
    outgoing_tx: mpsc::UnboundedSender<agent_client_protocol_core::jsonrpcmsg::Message>,
}

impl RegisteredSession {
    fn new(outgoing_tx: mpsc::UnboundedSender<agent_client_protocol_core::jsonrpcmsg::Message>) -> Self {
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

    // Parse incoming JSON-RPC message
    let message: agent_client_protocol_core::jsonrpcmsg::Message =
        serde_json::from_str(&body).map_err(agent_client_protocol_core::util::parse_error)?;

    match message {
        Message::Request(request) if request.id.is_some() => {
            tracing::debug!(%http_request_id, method = %request.method, "POST request received");
            // Request with id - return SSE stream for response
            let (tx, mut rx) = mpsc::unbounded();
            state
                .registration_tx
                .unbounded_send(HttpMessage::Request {
                    http_request_id,
                    request,
                    response_tx: tx,
                })
                .map_err(agent_client_protocol_core::util::internal_error)?;

            let stream = async_stream::stream! {
                while let Some(message) = rx.next().await {
                    tracing::debug!(%http_request_id, "sending SSE event");
                    match axum::response::sse::Event::default().json_data(message) {
                        Ok(v) => yield Ok(v),
                        Err(e) => yield Err(HttpError::from(e)),
                    }
                }
                tracing::debug!(%http_request_id, "SSE stream completed");
            };
            Ok(Sse::new(stream).into_response())
        }

        Message::Request(request) => {
            tracing::debug!(%http_request_id, method = %request.method, "POST notification received");
            // Request without id is a notification
            state
                .registration_tx
                .unbounded_send(HttpMessage::Notification {
                    http_request_id,
                    request,
                })
                .map_err(agent_client_protocol_core::util::internal_error)?;
            Ok(StatusCode::ACCEPTED.into_response())
        }

        Message::Response(response) => {
            tracing::debug!(%http_request_id, "POST response received");
            // Response from client (rare, but possible in MCP)
            state
                .registration_tx
                .unbounded_send(HttpMessage::Response {
                    http_request_id,
                    response,
                })
                .map_err(agent_client_protocol_core::util::internal_error)?;
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
    tracing::debug!(%http_request_id, "GET request received");

    let (tx, mut rx) = mpsc::unbounded();
    state
        .registration_tx
        .unbounded_send(HttpMessage::Get {
            http_request_id,
            response_tx: tx,
        })
        .map_err(agent_client_protocol_core::util::internal_error)?;

    let stream = async_stream::stream! {
        while let Some(message) = rx.next().await {
            tracing::debug!(%http_request_id, "sending SSE event");
            match axum::response::sse::Event::default().json_data(message) {
                Ok(v) => yield Ok(v),
                Err(e) => yield Err(HttpError::from(e)),
            }
        }
        tracing::debug!(%http_request_id, "SSE stream completed");
    };

    Ok(Sse::new(stream))
}
