//! HTTP-based MCP bridge transport.

use agent_client_protocol::{
    BoxFuture, Channel, ConnectTo, RawJsonRpcMessage, RawJsonRpcParams, TransportBatchEntry,
    TransportFrame,
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
        response_tx: mpsc::UnboundedSender<TransportFrame>,
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
    /// A batch retained as one transport frame.
    Frame {
        http_request_id: uuid::Uuid,
        frame: TransportFrame,
        request_ids: Vec<RequestId>,
        response_tx: Option<mpsc::UnboundedSender<TransportFrame>>,
    },
    /// A GET request to open an SSE stream for server-initiated messages.
    Get {
        http_request_id: uuid::Uuid,
        response_tx: mpsc::UnboundedSender<TransportFrame>,
    },
}

struct RunningServer {
    waiting_sessions: FxHashMap<RequestId, RegisteredSession>,
    waiting_batch_sessions: Vec<WaitingBatchSession>,
    general_sessions: Vec<RegisteredSession>,
    message_deque: VecDeque<TransportFrame>,
}

impl RunningServer {
    fn new() -> Self {
        RunningServer {
            waiting_sessions: HashMap::default(),
            waiting_batch_sessions: Vec::new(),
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
            FromChannelToHttp(TransportFrame),
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
        channel_tx: &mut mpsc::UnboundedSender<TransportFrame>,
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
                    .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::Request(request)))
                    .map_err(agent_client_protocol::util::internal_error)?;
                let session = RegisteredSession::new(response_tx);
                self.waiting_sessions.insert(request_id, session);
            }
            HttpMessage::Notification {
                http_request_id: _,
                request,
            } => {
                channel_tx
                    .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::Notification(
                        request,
                    )))
                    .map_err(agent_client_protocol::util::internal_error)?;
            }
            HttpMessage::Response {
                http_request_id: _,
                response,
            } => {
                channel_tx
                    .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::Response(
                        response,
                    )))
                    .map_err(agent_client_protocol::util::internal_error)?;
            }
            HttpMessage::Frame {
                http_request_id,
                frame,
                request_ids,
                response_tx,
            } => {
                tracing::debug!(%http_request_id, ?frame, "handling retained frame");
                if let Some(response_tx) = response_tx {
                    let session = RegisteredSession::new(response_tx);
                    match &frame {
                        TransportFrame::Batch(_) => {
                            self.waiting_batch_sessions.push(WaitingBatchSession {
                                request_ids,
                                session,
                            });
                        }
                        TransportFrame::Single(_) | TransportFrame::Malformed { .. } => {
                            unreachable!("only batches use the retained frame variant")
                        }
                    }
                }
                channel_tx
                    .unbounded_send(frame)
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
        mut message: TransportFrame,
    ) -> Option<TransportFrame> {
        if matches!(message, TransportFrame::Malformed { .. }) {
            // Malformed frames emitted by a relay are wire data, not protocol
            // responses, so they are delivered through a general stream.
        } else if matches!(message, TransportFrame::Batch(_)) {
            let response_ids: Vec<_> = match &message {
                TransportFrame::Batch(batch) => batch
                    .entries()
                    .filter_map(|entry| match entry {
                        TransportBatchEntry::Message(message) => message.response_id(),
                        TransportBatchEntry::Malformed { .. } => None,
                    })
                    .collect(),
                _ => unreachable!(),
            };
            let correlated = self.waiting_batch_sessions.iter().position(|waiting| {
                !waiting.request_ids.is_empty()
                    && waiting
                        .request_ids
                        .iter()
                        .any(|id| response_ids.contains(&id))
            });
            let fallback = self
                .waiting_batch_sessions
                .iter()
                .position(|waiting| waiting.request_ids.is_empty());
            if let Some(index) = correlated.or(fallback) {
                let session = self.waiting_batch_sessions.remove(index).session;
                match session.outgoing_tx.unbounded_send(message) {
                    Ok(()) => return None,
                    Err(error) => {
                        assert!(error.is_disconnected());
                        message = error.into_inner();
                    }
                }
            }
        }

        let message_id = match &message {
            TransportFrame::Single(message) => message.response_id().cloned(),
            TransportFrame::Malformed { .. } => None,
            TransportFrame::Batch(batch) => batch.entries().find_map(|entry| match entry {
                TransportBatchEntry::Message(message) => message.response_id().cloned(),
                TransportBatchEntry::Malformed { .. } => None,
            }),
        };

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
            .chain(self.waiting_sessions.values_mut())
            .chain(
                self.waiting_batch_sessions
                    .iter_mut()
                    .map(|waiting| &mut waiting.session),
            );
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
        self.waiting_batch_sessions
            .retain(|waiting| !waiting.session.outgoing_tx.is_closed());
    }
}

struct WaitingBatchSession {
    request_ids: Vec<RequestId>,
    session: RegisteredSession,
}

struct RegisteredSession {
    #[allow(dead_code)]
    id: uuid::Uuid,
    outgoing_tx: mpsc::UnboundedSender<TransportFrame>,
}

impl RegisteredSession {
    fn new(outgoing_tx: mpsc::UnboundedSender<TransportFrame>) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            outgoing_tx,
        }
    }
}

/// Accept a POST request carrying a JSON-RPC frame from an MCP client.
/// For response-bearing calls and batches, we return an SSE stream. For
/// notification/response-only frames, we return 202 Accepted.
async fn handle_post(
    State(state): State<Arc<BridgeState>>,
    body: String,
) -> Result<Response, HttpError> {
    let http_request_id = uuid::Uuid::new_v4();
    let Some(frame) = TransportFrame::parse_json(&body) else {
        return Ok(StatusCode::ACCEPTED.into_response());
    };

    match frame {
        TransportFrame::Single(message) => match message {
            RawJsonRpcMessage::Request(request) => {
                let (tx, rx) = mpsc::unbounded();
                state
                    .registration_tx
                    .unbounded_send(HttpMessage::Request {
                        http_request_id,
                        request,
                        response_tx: tx,
                    })
                    .map_err(agent_client_protocol::util::internal_error)?;

                Ok(sse_response(rx))
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
        },
        TransportFrame::Malformed { error, .. } => Ok(immediate_sse_response(
            TransportFrame::Single(RawJsonRpcMessage::response(RequestId::Null, Err(error))),
        )),
        TransportFrame::Batch(batch) => {
            if batch
                .entries()
                .all(|entry| matches!(entry, TransportBatchEntry::Malformed { .. }))
            {
                let responses = agent_client_protocol::TransportBatch::from_messages(
                    batch.entries().map(|entry| {
                        let TransportBatchEntry::Malformed { error, .. } = entry else {
                            unreachable!("all batch entries were checked as malformed")
                        };
                        RawJsonRpcMessage::response(RequestId::Null, Err(error.clone()))
                    }),
                )
                .expect("a TransportBatch is non-empty");
                return Ok(immediate_sse_response(TransportFrame::Batch(responses)));
            }

            let mut request_ids = Vec::new();
            let mut expects_response = false;
            for entry in batch.entries() {
                match entry {
                    TransportBatchEntry::Message(RawJsonRpcMessage::Request(request)) => {
                        request_ids.push(request.id.clone());
                        expects_response = true;
                    }
                    TransportBatchEntry::Malformed { .. } => expects_response = true,
                    TransportBatchEntry::Message(
                        RawJsonRpcMessage::Notification(_) | RawJsonRpcMessage::Response(_),
                    ) => {}
                }
            }
            let frame = TransportFrame::Batch(batch);
            if expects_response {
                let (tx, rx) = mpsc::unbounded();
                state
                    .registration_tx
                    .unbounded_send(HttpMessage::Frame {
                        http_request_id,
                        frame,
                        request_ids,
                        response_tx: Some(tx),
                    })
                    .map_err(agent_client_protocol::util::internal_error)?;
                Ok(sse_response(rx))
            } else {
                state
                    .registration_tx
                    .unbounded_send(HttpMessage::Frame {
                        http_request_id,
                        frame,
                        request_ids,
                        response_tx: None,
                    })
                    .map_err(agent_client_protocol::util::internal_error)?;
                Ok(StatusCode::ACCEPTED.into_response())
            }
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
            yield sse_event(message);
        }
    };

    Ok(Sse::new(stream))
}

fn sse_event(frame: TransportFrame) -> Result<axum::response::sse::Event, HttpError> {
    Ok(axum::response::sse::Event::default().data(frame.to_json()?))
}

fn sse_response(mut rx: mpsc::UnboundedReceiver<TransportFrame>) -> Response {
    let stream = async_stream::stream! {
        while let Some(message) = rx.next().await {
            yield sse_event(message);
        }
    };
    Sse::new(stream).into_response()
}

fn immediate_sse_response(frame: TransportFrame) -> Response {
    Sse::new(futures::stream::once(async move { sse_event(frame) })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn single_sse_payload(response: Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("SSE response body");
        let body = std::str::from_utf8(&body).expect("UTF-8 SSE response");
        let payload = body
            .lines()
            .find_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .expect("one SSE data event");
        serde_json::from_str(payload).expect("JSON-RPC SSE payload")
    }

    async fn single_sse_message(response: Response) -> RawJsonRpcMessage {
        serde_json::from_value(single_sse_payload(response).await)
            .expect("single JSON-RPC SSE message")
    }

    #[test]
    fn malformed_post_cannot_steal_a_valid_null_id_response() {
        futures::executor::block_on(async {
            let (registration_tx, mut registration_rx) = mpsc::unbounded();
            let state = Arc::new(BridgeState { registration_tx });

            let valid_http_response = handle_post(
                State(state.clone()),
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "method": "example",
                    "params": {}
                })
                .to_string(),
            )
            .await
            .expect("valid null-ID POST");
            let malformed_http_response = handle_post(State(state), "{not json".to_owned())
                .await
                .expect("malformed POST receives a JSON-RPC error");

            let valid_request = registration_rx
                .next()
                .await
                .expect("valid request is forwarded");
            assert!(
                registration_rx.try_recv().is_err(),
                "malformed input must be answered by its own HTTP request"
            );

            let mut server = RunningServer::new();
            let (mut channel_tx, mut channel_rx) = mpsc::unbounded();
            server
                .handle_http_message(valid_request, &mut channel_tx)
                .expect("forward valid request");
            assert!(matches!(
                channel_rx.next().await,
                Some(TransportFrame::Single(RawJsonRpcMessage::Request(_)))
            ));

            let valid_response = TransportFrame::Single(RawJsonRpcMessage::response(
                RequestId::Null,
                Ok(serde_json::json!({ "source": "valid" })),
            ));
            assert!(
                server
                    .try_dispatch_jsonrpc_message(valid_response)
                    .is_none()
            );

            assert!(matches!(
                single_sse_message(valid_http_response).await,
                RawJsonRpcMessage::Response(RpcResponse::Result {
                    id: RequestId::Null,
                    result,
                    ..
                }) if result == serde_json::json!({ "source": "valid" })
            ));
            assert!(matches!(
                single_sse_message(malformed_http_response).await,
                RawJsonRpcMessage::Response(RpcResponse::Error {
                    id: RequestId::Null,
                    error,
                    ..
                }) if error.code == agent_client_protocol::ErrorCode::ParseError
            ));
        });
    }

    #[test]
    fn forwards_batch_and_routes_grouped_response_without_flattening() {
        futures::executor::block_on(async {
            let mut server = RunningServer::new();
            let (mut channel_tx, mut channel_rx) = mpsc::unbounded();
            let (response_tx, mut response_rx) = mpsc::unbounded();
            let incoming = TransportFrame::Batch(
                agent_client_protocol::TransportBatch::from_messages([RawJsonRpcMessage::request(
                    "example".into(),
                    serde_json::json!({}),
                    RequestId::Number(7),
                )
                .unwrap()])
                .unwrap(),
            );

            server
                .handle_http_message(
                    HttpMessage::Frame {
                        http_request_id: uuid::Uuid::new_v4(),
                        frame: incoming,
                        request_ids: vec![RequestId::Number(7)],
                        response_tx: Some(response_tx),
                    },
                    &mut channel_tx,
                )
                .unwrap();
            assert!(matches!(
                channel_rx.next().await,
                Some(TransportFrame::Batch(_))
            ));

            let callback = TransportFrame::Single(
                RawJsonRpcMessage::notification("callback".into(), serde_json::json!({})).unwrap(),
            );
            assert!(server.try_dispatch_jsonrpc_message(callback).is_none());
            assert!(matches!(
                response_rx.next().await,
                Some(TransportFrame::Single(RawJsonRpcMessage::Notification(_)))
            ));

            let frame = TransportFrame::Batch(
                agent_client_protocol::TransportBatch::from_messages([
                    RawJsonRpcMessage::response(
                        RequestId::Number(7),
                        Ok(serde_json::json!({ "ok": true })),
                    ),
                ])
                .unwrap(),
            );
            let expected = frame.to_json().unwrap();

            assert!(server.try_dispatch_jsonrpc_message(frame).is_none());
            let received = response_rx
                .next()
                .await
                .expect("waiting HTTP request stays open");
            assert_eq!(received.to_json().unwrap(), expected);
            assert!(matches!(received, TransportFrame::Batch(_)));
        });
    }

    #[test]
    fn batch_post_round_trips_as_one_grouped_sse_response() {
        futures::executor::block_on(async {
            let (registration_tx, mut registration_rx) = mpsc::unbounded();
            let state = Arc::new(BridgeState { registration_tx });
            let incoming = serde_json::json!([
                {
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "example/first",
                    "params": {}
                },
                {
                    "jsonrpc": "2.0",
                    "id": 8,
                    "method": "example/second",
                    "params": {}
                }
            ]);

            let http_response = handle_post(State(state), incoming.to_string())
                .await
                .expect("batch POST should open an SSE response");
            let registration = registration_rx
                .next()
                .await
                .expect("batch POST should register with the bridge");

            let mut server = RunningServer::new();
            let (mut channel_tx, mut channel_rx) = mpsc::unbounded();
            server
                .handle_http_message(registration, &mut channel_tx)
                .expect("batch POST should be forwarded to the channel");
            let forwarded = channel_rx
                .next()
                .await
                .expect("channel should receive the batch frame");
            assert!(matches!(&forwarded, TransportFrame::Batch(_)));
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&forwarded.to_json().unwrap()).unwrap(),
                incoming
            );

            let response = TransportFrame::Batch(
                agent_client_protocol::TransportBatch::from_messages([
                    RawJsonRpcMessage::response(
                        RequestId::Number(7),
                        Ok(serde_json::json!({ "source": "first" })),
                    ),
                    RawJsonRpcMessage::response(
                        RequestId::Number(8),
                        Ok(serde_json::json!({ "source": "second" })),
                    ),
                ])
                .expect("grouped response should be non-empty"),
            );
            assert!(server.try_dispatch_jsonrpc_message(response).is_none());

            let payload = single_sse_payload(http_response).await;
            let entries = payload
                .as_array()
                .expect("SSE payload should remain one JSON-RPC array");
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0]["id"], 7);
            assert_eq!(entries[0]["result"]["source"], "first");
            assert_eq!(entries[1]["id"], 8);
            assert_eq!(entries[1]["result"]["source"], "second");
        });
    }

    #[test]
    fn malformed_batch_cannot_steal_a_valid_null_id_batch_response() {
        futures::executor::block_on(async {
            let (registration_tx, mut registration_rx) = mpsc::unbounded();
            let state = Arc::new(BridgeState { registration_tx });

            let valid_http_response = handle_post(
                State(state.clone()),
                serde_json::json!([{
                    "jsonrpc": "2.0",
                    "id": null,
                    "method": "example",
                    "params": {}
                }])
                .to_string(),
            )
            .await
            .expect("valid null-ID batch POST");
            let malformed_http_response = handle_post(State(state), "[17,false]".to_owned())
                .await
                .expect("malformed batch receives its own JSON-RPC error array");

            let valid_batch = registration_rx
                .next()
                .await
                .expect("valid batch is forwarded");
            assert!(
                registration_rx.try_recv().is_err(),
                "malformed-only batch must not register a bridge waiter"
            );

            let mut server = RunningServer::new();
            let (mut channel_tx, mut channel_rx) = mpsc::unbounded();
            server
                .handle_http_message(valid_batch, &mut channel_tx)
                .expect("forward valid null-ID batch");
            assert!(matches!(
                channel_rx.next().await,
                Some(TransportFrame::Batch(_))
            ));

            let valid_response = TransportFrame::Batch(
                agent_client_protocol::TransportBatch::from_messages([
                    RawJsonRpcMessage::response(
                        RequestId::Null,
                        Ok(serde_json::json!({ "source": "valid" })),
                    ),
                ])
                .expect("valid response batch is non-empty"),
            );
            assert!(
                server
                    .try_dispatch_jsonrpc_message(valid_response)
                    .is_none()
            );

            let valid_payload = single_sse_payload(valid_http_response).await;
            let valid_entries = valid_payload
                .as_array()
                .expect("valid response should remain a batch");
            assert_eq!(valid_entries.len(), 1);
            assert_eq!(valid_entries[0]["id"], serde_json::Value::Null);
            assert_eq!(valid_entries[0]["result"]["source"], "valid");

            let malformed_payload = single_sse_payload(malformed_http_response).await;
            let malformed_entries = malformed_payload
                .as_array()
                .expect("malformed response should be an error batch");
            assert_eq!(malformed_entries.len(), 2);
            for entry in malformed_entries {
                assert_eq!(entry["id"], serde_json::Value::Null);
                assert_eq!(
                    entry["error"]["code"],
                    i32::from(agent_client_protocol::ErrorCode::InvalidRequest)
                );
            }
        });
    }
}
