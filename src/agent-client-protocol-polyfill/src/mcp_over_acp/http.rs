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
    pending_calls: VecDeque<PendingCall>,
    general_sessions: Vec<RegisteredSession>,
    message_deque: VecDeque<TransportFrame>,
}

impl RunningServer {
    fn new() -> Self {
        RunningServer {
            waiting_sessions: HashMap::default(),
            waiting_batch_sessions: Vec::new(),
            pending_calls: VecDeque::new(),
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
            self.activate_pending_calls(&mut channel.tx)?;
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
                self.send_or_queue_call(
                    PendingCall {
                        frame: TransportFrame::Single(RawJsonRpcMessage::Request(request)),
                        request_ids: vec![request_id],
                        session: RegisteredSession::new(response_tx),
                    },
                    channel_tx,
                )?;
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
                    match &frame {
                        TransportFrame::Batch(_) => {
                            self.send_or_queue_call(
                                PendingCall {
                                    frame,
                                    request_ids,
                                    session: RegisteredSession::new(response_tx),
                                },
                                channel_tx,
                            )?;
                            return Ok(());
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

    fn send_or_queue_call(
        &mut self,
        call: PendingCall,
        channel_tx: &mut mpsc::UnboundedSender<TransportFrame>,
    ) -> Result<(), agent_client_protocol::Error> {
        if self.call_conflicts_with_active(&call.request_ids) {
            tracing::debug!(
                request_ids = ?call.request_ids,
                "queueing HTTP call until overlapping request IDs are no longer in flight"
            );
            self.pending_calls.push_back(call);
            return Ok(());
        }

        self.activate_call(call, channel_tx)
    }

    fn activate_call(
        &mut self,
        call: PendingCall,
        channel_tx: &mut mpsc::UnboundedSender<TransportFrame>,
    ) -> Result<(), agent_client_protocol::Error> {
        let PendingCall {
            frame,
            request_ids,
            session,
        } = call;
        let is_batch = matches!(frame, TransportFrame::Batch(_));
        channel_tx
            .unbounded_send(frame)
            .map_err(agent_client_protocol::util::internal_error)?;

        if is_batch {
            self.waiting_batch_sessions.push(WaitingBatchSession {
                request_ids,
                session,
            });
        } else {
            let request_id = request_ids
                .into_iter()
                .next()
                .expect("single request calls always have one request ID");
            self.waiting_sessions.insert(request_id, session);
        }

        Ok(())
    }

    fn activate_pending_calls(
        &mut self,
        channel_tx: &mut mpsc::UnboundedSender<TransportFrame>,
    ) -> Result<(), agent_client_protocol::Error> {
        loop {
            let Some(call) = self.pending_calls.front() else {
                return Ok(());
            };
            if call.session.outgoing_tx.is_closed() {
                self.pending_calls.pop_front();
                continue;
            }
            if self.call_conflicts_with_active(&call.request_ids) {
                return Ok(());
            }

            let call = self
                .pending_calls
                .pop_front()
                .expect("pending call was checked above");
            self.activate_call(call, channel_tx)?;
        }
    }

    fn call_conflicts_with_active(&self, request_ids: &[RequestId]) -> bool {
        let unidentified_batch_is_active = self
            .waiting_batch_sessions
            .iter()
            .any(|waiting| waiting.request_ids.is_empty());

        if request_ids.is_empty() {
            // A response-bearing batch without a request ID (for example, a
            // notification plus an invalid scalar) receives a grouped error
            // response whose only ID is null. Keep those responses ordered,
            // including with explicit null-ID calls, because the wire response
            // does not otherwise carry enough provenance to distinguish them.
            return unidentified_batch_is_active || self.request_id_is_active(&RequestId::Null);
        }

        request_ids
            .iter()
            .any(|request_id| self.request_id_is_active(request_id))
            || unidentified_batch_is_active && request_ids.contains(&RequestId::Null)
    }

    fn request_id_is_active(&self, request_id: &RequestId) -> bool {
        self.waiting_sessions.contains_key(request_id)
            || self
                .waiting_batch_sessions
                .iter()
                .any(|waiting| waiting.request_ids.contains(request_id))
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
            let fallback = response_ids.contains(&&RequestId::Null).then(|| {
                self.waiting_batch_sessions
                    .iter()
                    .position(|waiting| waiting.request_ids.is_empty())
            });
            let fallback = fallback.flatten();
            if let Some(index) = correlated.or(fallback) {
                let session = self.waiting_batch_sessions.remove(index).session;
                // This response belongs to that HTTP POST even if its SSE
                // receiver has gone away. Never let it fall through to a
                // later request that reuses the same JSON-RPC ID.
                drop(session.outgoing_tx.unbounded_send(message));
                return None;
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
            // This response belongs to that HTTP POST even if its SSE
            // receiver has gone away. Never let it fall through to a later
            // request that reuses the same JSON-RPC ID.
            drop(session.outgoing_tx.unbounded_send(message));
            return None;
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
            )
            .chain(
                self.pending_calls
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
        self.pending_calls
            .retain(|call| !call.session.outgoing_tx.is_closed());

        // Calls already forwarded to the JSON-RPC peer stay registered until
        // their response arrives. Otherwise a late response could be routed
        // to a newer HTTP POST that reused the same request ID.
    }
}

struct PendingCall {
    frame: TransportFrame,
    request_ids: Vec<RequestId>,
    session: RegisteredSession,
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
    let frame = TransportFrame::parse_json(&body);

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
        TransportFrame::Malformed { raw, error } => {
            if raw
                .parse::<serde_json::Value>()
                .is_ok_and(|value| is_response_only_shape(&value))
            {
                return Ok(StatusCode::ACCEPTED.into_response());
            }
            Ok(immediate_sse_response(TransportFrame::Single(
                RawJsonRpcMessage::response(RequestId::Null, Err(error)),
            )))
        }
        TransportFrame::Batch(batch) => {
            if batch
                .entries()
                .all(|entry| matches!(entry, TransportBatchEntry::Malformed { .. }))
            {
                let responses = agent_client_protocol::TransportBatch::from_messages(
                    batch.entries().filter_map(|entry| {
                        let TransportBatchEntry::Malformed { raw, error } = entry else {
                            unreachable!("all batch entries were checked as malformed")
                        };
                        (!is_response_only_shape(raw)).then(|| {
                            RawJsonRpcMessage::response(RequestId::Null, Err(error.clone()))
                        })
                    }),
                );
                let Some(responses) = responses else {
                    return Ok(StatusCode::ACCEPTED.into_response());
                };
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
                    TransportBatchEntry::Malformed { raw, .. } => {
                        expects_response |= !is_response_only_shape(raw);
                    }
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

fn is_response_only_shape(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|object| {
        !object.contains_key("method")
            && (object.contains_key("result") || object.contains_key("error"))
    })
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
    fn malformed_response_shaped_posts_are_ignored_without_registration() {
        futures::executor::block_on(async {
            let (registration_tx, mut registration_rx) = mpsc::unbounded();
            let state = Arc::new(BridgeState { registration_tx });
            let malformed_response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": null,
                "error": { "code": -32603, "message": "Internal error" }
            });

            let single = handle_post(State(state.clone()), malformed_response.to_string())
                .await
                .expect("malformed response-shaped POST");
            assert_eq!(single.status(), StatusCode::ACCEPTED);

            let batch = handle_post(
                State(state),
                serde_json::Value::Array(vec![malformed_response]).to_string(),
            )
            .await
            .expect("malformed response-only batch POST");
            assert_eq!(batch.status(), StatusCode::ACCEPTED);
            assert!(
                registration_rx.try_recv().is_err(),
                "ignored responses must not be forwarded or register HTTP waiters"
            );
        });
    }

    #[test]
    fn malformed_response_sibling_does_not_hide_invalid_batch_value() {
        futures::executor::block_on(async {
            let (registration_tx, mut registration_rx) = mpsc::unbounded();
            let state = Arc::new(BridgeState { registration_tx });
            let response = handle_post(
                State(state),
                serde_json::json!([
                    17,
                    {
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": null,
                        "error": { "code": -32603, "message": "Internal error" }
                    }
                ])
                .to_string(),
            )
            .await
            .expect("mixed malformed batch POST");

            let payload = single_sse_payload(response).await;
            let entries = payload.as_array().expect("batch response array");
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0]["id"], serde_json::Value::Null);
            assert_eq!(
                entries[0]["error"]["code"],
                i32::from(agent_client_protocol::ErrorCode::InvalidRequest)
            );
            assert!(
                registration_rx.try_recv().is_err(),
                "an all-malformed batch is answered by its originating POST"
            );
        });
    }

    #[test]
    fn concurrent_null_id_posts_are_serialized_to_preserve_response_provenance() {
        futures::executor::block_on(async {
            let (registration_tx, mut registration_rx) = mpsc::unbounded();
            let state = Arc::new(BridgeState { registration_tx });

            let first_http_response = handle_post(
                State(state.clone()),
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "method": "example/first",
                    "params": {}
                })
                .to_string(),
            )
            .await
            .expect("first null-ID POST");
            let second_http_response = handle_post(
                State(state),
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "method": "example/second",
                    "params": {}
                })
                .to_string(),
            )
            .await
            .expect("second null-ID POST");

            let first_registration = registration_rx.next().await.unwrap();
            let second_registration = registration_rx.next().await.unwrap();
            let mut server = RunningServer::new();
            let (mut channel_tx, mut channel_rx) = mpsc::unbounded();

            server
                .handle_http_message(first_registration, &mut channel_tx)
                .unwrap();
            server
                .handle_http_message(second_registration, &mut channel_tx)
                .unwrap();
            assert!(matches!(
                channel_rx.next().await,
                Some(TransportFrame::Single(RawJsonRpcMessage::Request(request)))
                    if request.method.as_ref() == "example/first"
                        && request.id == RequestId::Null
            ));
            assert!(
                channel_rx.try_recv().is_err(),
                "an overlapping null-ID request must wait for the first response"
            );

            assert!(
                server
                    .try_dispatch_jsonrpc_message(TransportFrame::Single(
                        RawJsonRpcMessage::response(
                            RequestId::Null,
                            Ok(serde_json::json!({ "source": "first" })),
                        ),
                    ))
                    .is_none()
            );
            server.activate_pending_calls(&mut channel_tx).unwrap();
            assert!(matches!(
                channel_rx.next().await,
                Some(TransportFrame::Single(RawJsonRpcMessage::Request(request)))
                    if request.method.as_ref() == "example/second"
                        && request.id == RequestId::Null
            ));

            assert!(
                server
                    .try_dispatch_jsonrpc_message(TransportFrame::Single(
                        RawJsonRpcMessage::response(
                            RequestId::Null,
                            Ok(serde_json::json!({ "source": "second" })),
                        ),
                    ))
                    .is_none()
            );

            assert!(matches!(
                single_sse_message(first_http_response).await,
                RawJsonRpcMessage::Response(RpcResponse::Result {
                    id: RequestId::Null,
                    result,
                    ..
                }) if result == serde_json::json!({ "source": "first" })
            ));
            assert!(matches!(
                single_sse_message(second_http_response).await,
                RawJsonRpcMessage::Response(RpcResponse::Result {
                    id: RequestId::Null,
                    result,
                    ..
                }) if result == serde_json::json!({ "source": "second" })
            ));
        });
    }

    #[test]
    fn unidentified_batch_posts_are_serialized_to_preserve_response_provenance() {
        futures::executor::block_on(async {
            fn unidentified_batch(method: &str) -> TransportFrame {
                TransportFrame::parse_json(
                    &serde_json::json!([
                        {
                            "jsonrpc": "2.0",
                            "method": method,
                            "params": {}
                        },
                        17
                    ])
                    .to_string(),
                )
            }

            fn grouped_response(source: &str) -> TransportFrame {
                TransportFrame::Batch(
                    agent_client_protocol::TransportBatch::from_messages([
                        RawJsonRpcMessage::response(
                            RequestId::Null,
                            Ok(serde_json::json!({ "source": source })),
                        ),
                    ])
                    .expect("grouped response is non-empty"),
                )
            }

            let mut server = RunningServer::new();
            let (mut channel_tx, mut channel_rx) = mpsc::unbounded();
            let (first_tx, mut first_rx) = mpsc::unbounded();
            let (second_tx, mut second_rx) = mpsc::unbounded();

            let first_frame = unidentified_batch("example/first");
            let second_frame = unidentified_batch("example/second");
            let expected_first_frame = first_frame.to_json().unwrap();
            let expected_second_frame = second_frame.to_json().unwrap();
            server
                .handle_http_message(
                    HttpMessage::Frame {
                        http_request_id: uuid::Uuid::new_v4(),
                        frame: first_frame,
                        request_ids: Vec::new(),
                        response_tx: Some(first_tx),
                    },
                    &mut channel_tx,
                )
                .unwrap();
            server
                .handle_http_message(
                    HttpMessage::Frame {
                        http_request_id: uuid::Uuid::new_v4(),
                        frame: second_frame,
                        request_ids: Vec::new(),
                        response_tx: Some(second_tx),
                    },
                    &mut channel_tx,
                )
                .unwrap();

            assert_eq!(
                channel_rx.next().await.unwrap().to_json().unwrap(),
                expected_first_frame
            );
            assert!(
                channel_rx.try_recv().is_err(),
                "a second unidentified batch must wait for the first response"
            );

            let callback = TransportFrame::Batch(
                agent_client_protocol::TransportBatch::from_messages([
                    RawJsonRpcMessage::notification("callback".into(), serde_json::json!({}))
                        .unwrap(),
                ])
                .expect("callback batch is non-empty"),
            );
            assert!(server.try_dispatch_jsonrpc_message(callback).is_none());
            assert!(matches!(
                first_rx.next().await,
                Some(TransportFrame::Batch(_))
            ));

            let first_response = grouped_response("first");
            let expected_first_response = first_response.to_json().unwrap();
            assert!(
                server
                    .try_dispatch_jsonrpc_message(first_response)
                    .is_none()
            );
            assert_eq!(
                first_rx.next().await.unwrap().to_json().unwrap(),
                expected_first_response
            );

            server.activate_pending_calls(&mut channel_tx).unwrap();
            assert_eq!(
                channel_rx.next().await.unwrap().to_json().unwrap(),
                expected_second_frame
            );

            let second_response = grouped_response("second");
            let expected_second_response = second_response.to_json().unwrap();
            assert!(
                server
                    .try_dispatch_jsonrpc_message(second_response)
                    .is_none()
            );
            assert_eq!(
                second_rx.next().await.unwrap().to_json().unwrap(),
                expected_second_response
            );
        });
    }

    #[test]
    fn late_response_to_disconnected_post_cannot_reach_reused_id() {
        futures::executor::block_on(async {
            fn request(method: &str) -> RpcRequest<RawJsonRpcParams> {
                let RawJsonRpcMessage::Request(request) = RawJsonRpcMessage::request(
                    method.to_owned(),
                    serde_json::json!({}),
                    RequestId::Null,
                )
                .unwrap() else {
                    unreachable!("request constructor always returns a request")
                };
                request
            }

            let mut server = RunningServer::new();
            let (mut channel_tx, mut channel_rx) = mpsc::unbounded();
            let (first_tx, first_rx) = mpsc::unbounded();
            let (second_tx, mut second_rx) = mpsc::unbounded();

            server
                .handle_http_message(
                    HttpMessage::Request {
                        http_request_id: uuid::Uuid::new_v4(),
                        request: request("example/first"),
                        response_tx: first_tx,
                    },
                    &mut channel_tx,
                )
                .unwrap();
            server
                .handle_http_message(
                    HttpMessage::Request {
                        http_request_id: uuid::Uuid::new_v4(),
                        request: request("example/second"),
                        response_tx: second_tx,
                    },
                    &mut channel_tx,
                )
                .unwrap();
            assert!(matches!(
                channel_rx.next().await,
                Some(TransportFrame::Single(RawJsonRpcMessage::Request(request)))
                    if request.method.as_ref() == "example/first"
            ));
            drop(first_rx);

            let callback = TransportFrame::Single(
                RawJsonRpcMessage::notification("callback".into(), serde_json::json!({})).unwrap(),
            );
            assert!(server.try_dispatch_jsonrpc_message(callback).is_none());
            assert!(matches!(
                second_rx.next().await,
                Some(TransportFrame::Single(RawJsonRpcMessage::Notification(_)))
            ));

            let first_response = TransportFrame::Single(RawJsonRpcMessage::response(
                RequestId::Null,
                Ok(serde_json::json!({ "source": "first" })),
            ));
            assert!(
                server
                    .try_dispatch_jsonrpc_message(first_response)
                    .is_none()
            );
            server.activate_pending_calls(&mut channel_tx).unwrap();
            assert!(matches!(
                channel_rx.next().await,
                Some(TransportFrame::Single(RawJsonRpcMessage::Request(request)))
                    if request.method.as_ref() == "example/second"
            ));

            let second_response = TransportFrame::Single(RawJsonRpcMessage::response(
                RequestId::Null,
                Ok(serde_json::json!({ "source": "second" })),
            ));
            assert!(
                server
                    .try_dispatch_jsonrpc_message(second_response)
                    .is_none()
            );
            assert!(matches!(
                second_rx.next().await,
                Some(TransportFrame::Single(RawJsonRpcMessage::Response(
                    RpcResponse::Result { result, .. }
                ))) if result == serde_json::json!({ "source": "second" })
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
