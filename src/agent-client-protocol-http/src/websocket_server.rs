use std::sync::Arc;

use agent_client_protocol::{RawJsonRpcMessage, TransportFrame};
use axum::{
    extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    http::HeaderValue,
    response::Response,
};
use futures::{SinkExt, StreamExt};
use tracing::{debug, error, info, trace, warn};

use crate::{
    connection::{ConnectionRegistry, OutboundLease},
    protocol::{HEADER_CONNECTION_ID, session_id_from_message},
};

pub(crate) fn handle_ws_upgrade(
    registry: Arc<ConnectionRegistry>,
    ws: WebSocketUpgrade,
) -> Response {
    let connection_id = ConnectionRegistry::next_connection_id();
    let conn_id_for_handler = connection_id.clone();
    let registry_for_handler = registry.clone();
    let mut response = ws.on_upgrade(move |socket| async move {
        let connection = registry_for_handler
            .create_websocket_connection_with_id(conn_id_for_handler.clone())
            .await;
        connection.start_router().await;
        info!(connection_id = %conn_id_for_handler, "WebSocket connection created");
        run_ws(
            socket,
            registry_for_handler,
            conn_id_for_handler,
            connection,
        )
        .await;
    });

    if let Ok(v) = HeaderValue::from_str(&connection_id) {
        response.headers_mut().insert(HEADER_CONNECTION_ID, v);
    }
    response
}

async fn run_ws(
    socket: WebSocket,
    registry: Arc<ConnectionRegistry>,
    connection_id: String,
    connection: Arc<crate::connection::Connection>,
) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let Some(mut outbound_rx) = connection.subscribe_all_outbound() else {
        error!(connection_id = %connection_id, "WebSocket outbound mailbox already subscribed");
        if let Some(conn) = registry.remove(&connection_id).await {
            conn.shutdown().await;
        }
        return;
    };
    let mut closed = connection.subscribe_closed();

    debug!(connection_id = %connection_id, "Starting WebSocket message loop");

    run_ws_message_loop(
        &mut ws_tx,
        &mut ws_rx,
        &mut outbound_rx,
        &mut closed,
        &connection_id,
        &connection,
    )
    .await;

    debug!(connection_id = %connection_id, "Cleaning up WebSocket connection");
    if let Some(conn) = registry.remove(&connection_id).await {
        conn.shutdown().await;
    }
}

async fn run_ws_message_loop(
    ws_tx: &mut futures::stream::SplitSink<WebSocket, WsMessage>,
    ws_rx: &mut futures::stream::SplitStream<WebSocket>,
    outbound_rx: &mut OutboundLease,
    closed: &mut tokio::sync::watch::Receiver<bool>,
    connection_id: &str,
    connection: &crate::connection::Connection,
) {
    loop {
        if *closed.borrow() {
            drain_queued_outbound(ws_tx, outbound_rx, connection_id).await;
            break;
        }
        tokio::select! {
            recv = outbound_rx.recv() => {
                match recv {
                    Some(text) => {
                        if !send_outbound_text(ws_tx, text, connection_id).await {
                            break;
                        }
                    }
                    None => break,
                }
            }

            changed = closed.changed() => {
                if changed.is_err() || *closed.borrow() {
                    drain_queued_outbound(ws_tx, outbound_rx, connection_id).await;
                    break;
                }
            }

            msg_result = ws_rx.next() => {
                match msg_result {
                    Some(Ok(WsMessage::Text(text))) => {
                        if !forward_client_text(
                            text.to_string(),
                            ws_tx,
                            outbound_rx,
                            closed,
                            connection_id,
                            connection,
                        )
                        .await
                        {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(frame))) => {
                        debug!(connection_id = %connection_id, "Client closed connection: {:?}", frame);
                        break;
                    }
                    Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => {}
                    Some(Ok(WsMessage::Binary(_))) => {
                        warn!(connection_id = %connection_id, "Ignoring binary message (ACP uses text)");
                    }
                    Some(Err(e)) => {
                        error!(connection_id = %connection_id, "WebSocket error: {e}");
                        break;
                    }
                    None => break,
                }
            }
        }
    }
}

async fn forward_client_text<S>(
    text: String,
    ws_tx: &mut S,
    outbound_rx: &mut OutboundLease,
    closed: &mut tokio::sync::watch::Receiver<bool>,
    connection_id: &str,
    connection: &crate::connection::Connection,
) -> bool
where
    S: futures::Sink<WsMessage> + Unpin,
{
    trace!(connection_id = %connection_id, payload = %text, "Client → Agent: {} bytes", text.len());
    let frame = TransportFrame::parse_json(&text);
    if let TransportFrame::Single(parsed) = &frame
        && let Some(sid) = session_id_from_message(parsed)
        && let RawJsonRpcMessage::Request(req) = parsed
    {
        trace!(connection_id = %connection_id, session_id = %sid, request_id = ?req.id, "Client → Agent (session)");
    }
    if connection.send_frame_to_agent(frame).is_err() {
        error!(connection_id = %connection_id, "Agent channel closed");
        drain_outbound_until_closed(ws_tx, outbound_rx, closed, connection_id).await;
        false
    } else {
        true
    }
}

async fn drain_outbound_until_closed<S>(
    ws_tx: &mut S,
    outbound_rx: &mut OutboundLease,
    closed: &mut tokio::sync::watch::Receiver<bool>,
    connection_id: &str,
) where
    S: futures::Sink<WsMessage> + Unpin,
{
    loop {
        drain_queued_outbound(ws_tx, outbound_rx, connection_id).await;
        if *closed.borrow() {
            drain_queued_outbound(ws_tx, outbound_rx, connection_id).await;
            break;
        }
        tokio::select! {
            biased;
            recv = outbound_rx.recv() => match recv {
                Some(text) => {
                    if !send_outbound_text(ws_tx, text, connection_id).await {
                        break;
                    }
                }
                None => break,
            },
            changed = closed.changed() => {
                if changed.is_err() || *closed.borrow() {
                    drain_queued_outbound(ws_tx, outbound_rx, connection_id).await;
                    break;
                }
            }
        }
    }
}

async fn drain_queued_outbound<S>(
    ws_tx: &mut S,
    outbound_rx: &mut OutboundLease,
    connection_id: &str,
) where
    S: futures::Sink<WsMessage> + Unpin,
{
    while let Ok(text) = outbound_rx.try_recv() {
        if !send_outbound_text(ws_tx, text, connection_id).await {
            break;
        }
    }
}

async fn send_outbound_text<S>(ws_tx: &mut S, text: String, connection_id: &str) -> bool
where
    S: futures::Sink<WsMessage> + Unpin,
{
    trace!(connection_id = %connection_id, payload = %text, "Agent → Client: {} bytes", text.len());
    if ws_tx.send(WsMessage::Text(text.into())).await.is_err() {
        error!(connection_id = %connection_id, "WebSocket send failed");
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        path::PathBuf,
        sync::{Mutex, OnceLock},
    };

    use agent_client_protocol::{
        Agent, Channel, Dispatch, TransportBatch, TransportBatchEntry, TransportFrame,
        UntypedMessage,
        schema::v1::{
            ReadTextFileRequest, ReadTextFileResponse, RequestId, Response as RpcResponse,
            SessionId,
        },
    };
    use async_tungstenite::{
        tokio::connect_async,
        tungstenite::{Message as ClientWsMessage, protocol::frame::coding::CloseCode},
    };
    use axum::{Router, extract::WebSocketUpgrade, routing::get};
    use futures::{StreamExt as _, future::BoxFuture};
    use serde_json::json;
    use tokio::{
        net::TcpListener,
        sync::mpsc,
        time::{Duration, timeout},
    };

    use crate::{
        AcpHttpServer, WebSocketLimits,
        connection::{AgentFactory, ConnectionRegistry},
    };

    use super::*;

    const ISSUE_288_BURST: usize = 1_025;
    const SENSITIVE_TEXT_CONTENT: &str = "TOP_SECRET_PROMPT_SENTINEL";
    const SENSITIVE_BASE64_CONTENT: &str = "TOP_SECRET_BASE64_SENTINEL";
    const SENSITIVE_REQUEST_ID_CONTENT: &str = "TOP_SECRET_REQUEST_ID_SENTINEL";
    const SENSITIVE_MALFORMED_CONTENT: &str = "TOP_SECRET_MALFORMED_SENTINEL";
    const SENSITIVE_TYPED_INVALID_TEXT_CONTENT: &str = "TOP_SECRET_TYPED_INVALID_PROMPT_SENTINEL";
    const SENSITIVE_TYPED_INVALID_BASE64_CONTENT: &str = "TOP_SECRET_TYPED_INVALID_BASE64_SENTINEL";
    const SENSITIVE_TYPED_RESPONSE_CONTENT: &str = "TOP_SECRET_TYPED_RESPONSE_SENTINEL";

    struct CapturingAgentFactory {
        forwarded: mpsc::UnboundedSender<RawJsonRpcMessage>,
    }

    #[derive(Clone)]
    struct SharedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedLogWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            io::Write::write(&mut *self.0.lock().unwrap(), buffer)
        }

        fn flush(&mut self) -> io::Result<()> {
            io::Write::flush(&mut *self.0.lock().unwrap())
        }
    }

    impl AgentFactory for CapturingAgentFactory {
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (agent, transport) = Channel::duplex();
            let forwarded = self.forwarded.clone();
            let future = Box::pin(async move {
                let Channel {
                    rx: mut incoming,
                    tx: outgoing,
                } = agent;
                while let Some(frame) = incoming.next().await {
                    match frame {
                        TransportFrame::Single(message) => {
                            if forwarded.send(message).is_err() {
                                break;
                            }
                        }
                        TransportFrame::Malformed { raw, error } => {
                            if !test_raw_is_response_only_shape(&raw) {
                                outgoing
                                    .unbounded_send(TransportFrame::Single(
                                        RawJsonRpcMessage::response(RequestId::Null, Err(error)),
                                    ))
                                    .unwrap();
                            }
                        }
                        TransportFrame::Batch(_) => panic!("expected a single JSON-RPC frame"),
                    }
                }
                Ok(())
            });

            (transport, future)
        }
    }

    struct BatchAgentFactory {
        forwarded: mpsc::UnboundedSender<Vec<String>>,
    }

    impl AgentFactory for BatchAgentFactory {
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (mut agent, transport) = Channel::duplex();
            let forwarded = self.forwarded.clone();
            let future = Box::pin(async move {
                let Some(TransportFrame::Batch(batch)) = agent.rx.next().await else {
                    panic!("expected one batch frame");
                };
                let mut methods = Vec::new();
                let mut responses = Vec::new();
                for entry in batch.entries() {
                    let TransportBatchEntry::Message(RawJsonRpcMessage::Request(request)) = entry
                    else {
                        panic!("expected a request batch entry");
                    };
                    methods.push(request.method.to_string());
                    responses.push(RawJsonRpcMessage::response(
                        request.id.clone(),
                        Ok(json!({ "ok": true })),
                    ));
                }
                forwarded.send(methods).unwrap();
                let responses =
                    TransportBatch::from_messages(responses).expect("responses are non-empty");
                agent
                    .tx
                    .unbounded_send(TransportFrame::Batch(responses))
                    .unwrap();
                std::future::pending::<agent_client_protocol::Result<()>>().await
            });

            (transport, future)
        }
    }

    struct FinalFrameThenExitAgentFactory {
        emit: Arc<tokio::sync::Notify>,
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

    struct FinalFrameAfterInputCloseAgentFactory {
        emit: Arc<tokio::sync::Notify>,
    }

    impl AgentFactory for FinalFrameAfterInputCloseAgentFactory {
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (agent, transport) = Channel::duplex();
            let emit = self.emit.clone();
            let future = Box::pin(async move {
                drop(agent.rx);
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

    fn text_prompt_request_with_size(id: i64, session_id: &str, target_size: usize) -> String {
        let request = |text: String| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": text}]
                }
            })
        };
        let empty_request = serde_json::to_string(&request(String::new())).unwrap();
        assert!(empty_request.len() <= target_size);
        let padding_size = target_size - empty_request.len();
        assert!(SENSITIVE_TEXT_CONTENT.len() <= padding_size);
        let text = format!(
            "{SENSITIVE_TEXT_CONTENT}{}",
            "x".repeat(padding_size - SENSITIVE_TEXT_CONTENT.len())
        );
        let request = serde_json::to_string(&request(text)).unwrap();
        assert_eq!(request.len(), target_size);
        request
    }

    fn null_id_text_prompt_request_with_size(session_id: &str, target_size: usize) -> String {
        let request = |text: String| {
            json!({
                "jsonrpc": "2.0",
                "id": null,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": text}]
                }
            })
        };
        let empty_request = serde_json::to_string(&request(String::new())).unwrap();
        assert!(empty_request.len() <= target_size);
        let padding_size = target_size - empty_request.len();
        assert!(SENSITIVE_TEXT_CONTENT.len() <= padding_size);
        let text = format!(
            "{SENSITIVE_TEXT_CONTENT}{}",
            "x".repeat(padding_size - SENSITIVE_TEXT_CONTENT.len())
        );
        let request = serde_json::to_string(&request(text)).unwrap();
        assert_eq!(request.len(), target_size);
        request
    }

    fn base64_prompt_request_with_size(id: i64, session_id: &str, target_size: usize) -> String {
        let request = |data: String| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{
                        "type": "image",
                        "data": data,
                        "mimeType": "image/png"
                    }]
                }
            })
        };
        let empty_request = serde_json::to_string(&request(String::new())).unwrap();
        assert!(empty_request.len() <= target_size);
        let padding_size = target_size - empty_request.len();
        assert!(SENSITIVE_BASE64_CONTENT.len() <= padding_size);
        let data = format!(
            "{SENSITIVE_BASE64_CONTENT}{}",
            "A".repeat(padding_size - SENSITIVE_BASE64_CONTENT.len())
        );
        let request = serde_json::to_string(&request(data)).unwrap();
        assert_eq!(request.len(), target_size);
        request
    }

    macro_rules! send_client_message {
        ($client:ident, $message:expr $(,)?) => {
            timeout(Duration::from_secs(1), $client.send($message))
                .await
                .expect("WebSocket send should not hang")
                .expect("WebSocket message should be sent")
        };
    }

    async fn spawn_capturing_server(
        limits: WebSocketLimits,
    ) -> (
        std::net::SocketAddr,
        Arc<ConnectionRegistry>,
        mpsc::UnboundedReceiver<RawJsonRpcMessage>,
        tokio::task::JoinHandle<()>,
    ) {
        let (forwarded_tx, forwarded_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(CapturingAgentFactory {
            forwarded: forwarded_tx,
        })));
        let app = Router::new().route(
            "/acp",
            get({
                let registry = registry.clone();
                move |ws: WebSocketUpgrade| {
                    let registry = registry.clone();
                    async move {
                        handle_ws_upgrade(
                            registry,
                            ws.max_frame_size(limits.max_frame_size())
                                .max_message_size(limits.max_message_size()),
                            Some(limits),
                        )
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, registry, forwarded_rx, server)
    }

    fn test_logs() -> Arc<Mutex<Vec<u8>>> {
        static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

        LOGS.get_or_init(|| {
            let logs = Arc::new(Mutex::new(Vec::new()));
            let writer = SharedLogWriter(logs.clone());
            tracing_subscriber::fmt()
                .with_env_filter("agent_client_protocol=trace,agent_client_protocol_http=trace")
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .try_init()
                .expect("test logging subscriber should initialize once");
            logs
        })
        .clone()
    }

    fn logs_for_connection(logs: &Mutex<Vec<u8>>, connection_id: &str) -> String {
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        logs.lines()
            .filter(|line| line.contains(connection_id))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn all_test_logs(logs: &Mutex<Vec<u8>>) -> String {
        String::from_utf8(logs.lock().unwrap().clone()).unwrap()
    }

    fn test_raw_is_response_only_shape(raw: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(raw).is_ok_and(|value| {
            value.as_object().is_some_and(|object| {
                !object.contains_key("method")
                    && (object.contains_key("result") || object.contains_key("error"))
            })
        })
    }

    #[test]
    fn soft_limit_batch_preflight_skips_notifications_and_response_shapes() {
        let batch = json!([
            {
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {}
            },
            {
                "jsonrpc": "2.0",
                "id": 80,
                "result": {"ok": true}
            },
            {
                "jsonrpc": "2.0",
                "id": 81,
                "result": {},
                "error": {"code": -1, "message": "conflicting response fields"}
            }
        ])
        .to_string();

        assert!(oversized_requests(&batch, batch.len() - 1, 4_096).is_none());
    }

    #[test]
    fn soft_limit_preflight_uses_null_id_for_invalid_single_and_empty_batch() {
        let invalid = json!({
            "jsonrpc": "2.0",
            "id": 82,
            "method": 123,
            "params": {}
        })
        .to_string();
        assert!(matches!(
            oversized_requests(&invalid, invalid.len() - 1, 4_096),
            Some(OversizedRequests::Single(RequestId::Null))
        ));

        let empty_batch = "[        ]";
        assert!(matches!(
            oversized_requests(empty_batch, empty_batch.len() - 1, 4_096),
            Some(OversizedRequests::Single(RequestId::Null))
        ));
    }

    #[test]
    fn soft_limit_batch_preflight_is_bounded_by_response_budget() {
        let batch = format!("[{}]", vec!["0"; 4_096].join(","));
        let max_request_bytes = batch.len() - 1;
        let max_response_bytes = 256;

        let Some(OversizedRequests::Batch {
            response,
            request_count: _,
        }) = oversized_requests(&batch, max_request_bytes, max_response_bytes)
        else {
            panic!("oversized invalid batch should be rejected as a batch");
        };
        assert!(matches!(
            response,
            BoundedSoftLimitResponse::TooLong { max_bytes: 256, .. }
        ));
    }

    #[tokio::test]
    async fn websocket_buffers_burst_without_polling_slow_subscriber() {
        let (forwarded_tx, _forwarded_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(CapturingAgentFactory {
            forwarded: forwarded_tx,
        })));
        let app = Router::new().route(
            "/acp",
            get({
                let registry = registry.clone();
                move |ws: WebSocketUpgrade| {
                    let registry = registry.clone();
                    async move {
                        ws.on_upgrade(move |socket| async move {
                            let connection_id = ConnectionRegistry::next_connection_id();
                            let connection = registry
                                .create_websocket_connection_with_id(connection_id.clone())
                                .await;
                            let mut outbound_rx = connection.subscribe_all_outbound().unwrap();

                            for index in 0..ISSUE_288_BURST {
                                connection
                                    .push_all_outbound_for_test(format!("message-{index}"))
                                    .unwrap();
                            }

                            let mut closed = connection.subscribe_closed();
                            let (mut ws_tx, mut ws_rx) = socket.split();
                            let message_loop = run_ws_message_loop(
                                &mut ws_tx,
                                &mut ws_rx,
                                &mut outbound_rx,
                                &mut closed,
                                &connection_id,
                                &connection,
                                None,
                            );
                            let finish = connection.shutdown();
                            futures::join!(message_loop, finish);
                            registry.remove(&connection_id).await;
                        })
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();

        timeout(Duration::from_secs(5), async {
            for index in 0..ISSUE_288_BURST {
                let frame = client.next().await.unwrap().unwrap();
                let ClientWsMessage::Text(text) = frame else {
                    panic!("expected text frame: {frame:?}");
                };
                assert_eq!(text, format!("message-{index}"));
            }
        })
        .await
        .expect("WebSocket should deliver the complete burst");

        server.abort();
    }

    #[tokio::test]
    async fn websocket_drains_final_agent_frame_before_closing() {
        let emit = Arc::new(tokio::sync::Notify::new());
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(
            FinalFrameThenExitAgentFactory { emit: emit.clone() },
        )));
        let app = Router::new().route(
            "/acp",
            get({
                let registry = registry.clone();
                move |ws: WebSocketUpgrade| {
                    let registry = registry.clone();
                    let emit = emit.clone();
                    async move {
                        ws.on_upgrade(move |socket| async move {
                            let connection_id = ConnectionRegistry::next_connection_id();
                            let connection = registry
                                .create_websocket_connection_with_id(connection_id.clone())
                                .await;
                            connection.start_router().await;
                            let mut outbound_rx = connection.subscribe_all_outbound().unwrap();
                            let mut closed = connection.subscribe_closed();

                            emit.notify_one();
                            while !*closed.borrow() {
                                closed.changed().await.unwrap();
                            }

                            let (mut ws_tx, mut ws_rx) = socket.split();
                            run_ws_message_loop(
                                &mut ws_tx,
                                &mut ws_rx,
                                &mut outbound_rx,
                                &mut closed,
                                &connection_id,
                                &connection,
                                None,
                            )
                            .await;

                            if let Some(connection) = registry.remove(&connection_id).await {
                                connection.shutdown().await;
                            }
                        })
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let ClientWsMessage::Text(text) = frame else {
            panic!("expected final text frame: {frame:?}");
        };
        let message = serde_json::from_str::<RawJsonRpcMessage>(&text).unwrap();
        assert!(matches!(
            message,
            RawJsonRpcMessage::Notification(notification)
                if notification.method.as_ref() == "test/final"
        ));
        let terminal = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("WebSocket should terminate after draining the final frame");
        match terminal {
            None | Some(Ok(ClientWsMessage::Close(_)) | Err(_)) => {}
            Some(Ok(frame)) => panic!("expected WebSocket termination, got {frame:?}"),
        }

        server.abort();
    }

    #[tokio::test]
    async fn inbound_after_agent_exit_drains_queued_final_frame() {
        let emit = Arc::new(tokio::sync::Notify::new());
        let registry = ConnectionRegistry::new(Arc::new(FinalFrameAfterInputCloseAgentFactory {
            emit: emit.clone(),
        }));
        let connection_id = ConnectionRegistry::next_connection_id();
        let connection = registry
            .create_websocket_connection_with_id(connection_id.clone())
            .await;
        connection.start_router().await;
        let mut outbound_rx = connection.subscribe_all_outbound().unwrap();
        let mut closed = connection.subscribe_closed();

        timeout(Duration::from_secs(1), async {
            loop {
                let probe = RawJsonRpcMessage::notification(
                    "test/probe".to_string(),
                    serde_json::json!({}),
                )
                .unwrap();
                if connection
                    .send_frame_to_agent(TransportFrame::Single(probe))
                    .is_err()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("agent input did not close");
        assert!(!*closed.borrow(), "outbound routing should still be active");

        let (mut ws_tx, mut ws_rx) = futures::channel::mpsc::unbounded::<WsMessage>();
        let inbound =
            RawJsonRpcMessage::notification("test/inbound".to_string(), serde_json::json!({}))
                .unwrap();
        let inbound = serde_json::to_string(&inbound).unwrap();
        let forward = forward_client_text(
            &inbound,
            &mut ws_tx,
            &mut outbound_rx,
            &mut closed,
            &connection_id,
            &connection,
            None,
        );
        futures::pin_mut!(forward);
        assert!(
            futures::poll!(&mut forward).is_pending(),
            "the WebSocket exited before the outbound router drained"
        );

        emit.notify_one();
        assert!(
            !timeout(Duration::from_secs(1), forward)
                .await
                .expect("WebSocket did not close after the outbound router drained"),
            "the closed agent channel should end the WebSocket loop"
        );

        let WsMessage::Text(text) = ws_rx.next().await.unwrap() else {
            panic!("expected queued final text frame");
        };
        let message = serde_json::from_str::<RawJsonRpcMessage>(&text).unwrap();
        assert!(matches!(
            message,
            RawJsonRpcMessage::Notification(notification)
                if notification.method.as_ref() == "test/final"
        ));

        registry.remove(&connection_id).await;
        connection.shutdown().await;
    }

    #[tokio::test]
    async fn malformed_ws_frame_returns_parse_error_response_and_continues() {
        let logs = test_logs();
        let (forwarded_tx, mut forwarded_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(CapturingAgentFactory {
            forwarded: forwarded_tx,
        })));
        let app = Router::new().route(
            "/acp",
            get({
                let registry = registry.clone();
                move |ws: WebSocketUpgrade| {
                    let registry = registry.clone();
                    async move { handle_ws_upgrade(registry, ws, None) }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let malformed = format!("{{not json {SENSITIVE_TEXT_CONTENT}");

        client
            .send(ClientWsMessage::Text(malformed.into()))
            .await
            .unwrap();

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let ClientWsMessage::Text(text) = frame else {
            panic!("expected text frame: {frame:?}");
        };
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["id"], serde_json::Value::Null);
        assert_eq!(value["error"]["code"], -32700);
        assert!(value["error"]["data"].is_null());
        assert!(!text.contains(SENSITIVE_TEXT_CONTENT));

        let parsed = serde_json::from_value::<RawJsonRpcMessage>(value).unwrap();
        assert!(matches!(
            parsed,
            RawJsonRpcMessage::Response(RpcResponse::Error {
                id: RequestId::Null,
                ..
            })
        ));

        let notification =
            RawJsonRpcMessage::notification("test/method".to_string(), json!({})).unwrap();
        client
            .send(ClientWsMessage::Text(
                serde_json::to_string(&notification).unwrap().into(),
            ))
            .await
            .unwrap();

        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            forwarded,
            RawJsonRpcMessage::Notification(notification)
                if notification.method.as_ref() == "test/method"
        ));

        let logs = all_test_logs(&logs);
        assert!(
            !logs.contains(SENSITIVE_TEXT_CONTENT),
            "malformed request content leaked into logs:\n{logs}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn malformed_response_shape_does_not_receive_error_and_connection_survives() {
        let logs = test_logs();
        let limits = WebSocketLimits::new(4_096, 4_096, 2_048);
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let malformed_response = json!({
            "jsonrpc": "2.0",
            "id": 61,
            "result": {"secret": SENSITIVE_BASE64_CONTENT},
            "error": {"code": -1, "message": "conflicting response fields"}
        });

        send_client_message!(
            client,
            ClientWsMessage::Text(serde_json::to_string(&malformed_response).unwrap().into()),
        );
        assert!(
            timeout(Duration::from_secs(1), client.next())
                .await
                .is_err(),
            "a malformed response shape must not receive a response"
        );

        let notification =
            RawJsonRpcMessage::notification("test/after-response".to_string(), json!({})).unwrap();
        send_client_message!(
            client,
            ClientWsMessage::Text(serde_json::to_string(&notification).unwrap().into()),
        );
        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .expect("connection should remain usable after a malformed response")
            .expect("agent channel should remain open");
        assert!(matches!(
            forwarded,
            RawJsonRpcMessage::Notification(notification)
                if notification.method.as_ref() == "test/after-response"
        ));

        let logs = all_test_logs(&logs);
        assert!(
            !logs.contains(SENSITIVE_BASE64_CONTENT),
            "malformed response content leaked into logs:\n{logs}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn websocket_forwards_batch_as_one_frame_and_emits_grouped_response() {
        let (forwarded_tx, mut forwarded_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(BatchAgentFactory {
            forwarded: forwarded_tx,
        })));
        let app = Router::new().route(
            "/acp",
            get({
                let registry = registry.clone();
                move |ws: WebSocketUpgrade| {
                    let registry = registry.clone();
                    async move { handle_ws_upgrade(registry, ws, None) }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let batch = json!([
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "custom/first",
                "params": {}
            },
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "custom/second",
                "params": {}
            }
        ]);

        client
            .send(ClientWsMessage::Text(batch.to_string().into()))
            .await
            .unwrap();

        let methods = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(methods, ["custom/first", "custom/second"]);
        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let ClientWsMessage::Text(text) = frame else {
            panic!("expected text frame: {frame:?}");
        };
        let response = serde_json::from_str::<serde_json::Value>(&text).unwrap();
        let entries = response.as_array().expect("response should remain a batch");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["id"], 1);
        assert_eq!(entries[1]["id"], 2);

        server.abort();
    }

    #[tokio::test]
    async fn oversized_single_request_batch_returns_batch_error_and_connection_survives() {
        const MAX_JSON_RPC_REQUEST_SIZE: usize = 1_024;

        let logs = test_logs();
        let limits = WebSocketLimits::new(4_096, 4_096, MAX_JSON_RPC_REQUEST_SIZE);
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, response) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let connection_id = response
            .headers()
            .get(HEADER_CONNECTION_ID)
            .expect("upgrade should include a connection ID")
            .to_str()
            .unwrap()
            .to_owned();
        let request =
            text_prompt_request_with_size(31, "session-batch-a", MAX_JSON_RPC_REQUEST_SIZE - 1);
        let batch = format!("[{request}]");
        assert_eq!(batch.len(), MAX_JSON_RPC_REQUEST_SIZE + 1);

        send_client_message!(client, ClientWsMessage::Text(batch.into()));

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("server should reject an oversized JSON-RPC batch")
            .expect("connection should remain open")
            .expect("soft-limit response should be readable");
        let ClientWsMessage::Text(text) = frame else {
            panic!("expected text response, got {frame:?}");
        };
        let response: serde_json::Value = serde_json::from_str(&text).unwrap();
        let entries = response.as_array().expect("response should remain a batch");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], 31);
        assert_eq!(entries[0]["error"]["code"], -32600);
        assert_eq!(entries[0]["error"]["data"]["kind"], "payload_too_large");
        assert!(
            timeout(Duration::from_secs(1), forwarded_rx.recv())
                .await
                .is_err(),
            "oversized batch must not be forwarded"
        );

        let request = json!({
            "jsonrpc": "2.0",
            "id": 32,
            "method": "session/prompt",
            "params": {
                "sessionId": "session-b",
                "prompt": [{"type": "text", "text": "small"}]
            }
        });
        send_client_message!(
            client,
            ClientWsMessage::Text(serde_json::to_string(&request).unwrap().into()),
        );
        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .expect("small request should be forwarded on the same connection")
            .expect("agent channel should remain open");
        assert!(matches!(
            forwarded,
            RawJsonRpcMessage::Request(request) if request.id == RequestId::Number(32)
        ));

        let logs = logs_for_connection(&logs, &connection_id);
        assert!(logs.contains("Rejecting oversized JSON-RPC request batch"));
        assert!(!logs.contains(SENSITIVE_TEXT_CONTENT));

        server.abort();
    }

    #[tokio::test]
    async fn oversized_mixed_batch_returns_errors_for_request_ids_only() {
        const MAX_JSON_RPC_REQUEST_SIZE: usize = 1_024;
        const TARGET_SIZE: usize = MAX_JSON_RPC_REQUEST_SIZE + 1;

        let logs = test_logs();
        let limits = WebSocketLimits::new(4_096, 4_096, MAX_JSON_RPC_REQUEST_SIZE);
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, response) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let connection_id = response
            .headers()
            .get(HEADER_CONNECTION_ID)
            .expect("upgrade should include a connection ID")
            .to_str()
            .unwrap()
            .to_owned();
        let batch = |padding: String| {
            json!([
                {
                    "jsonrpc": "2.0",
                    "id": 41,
                    "method": "session/prompt",
                    "params": {"prompt": padding}
                },
                {
                    "jsonrpc": "2.0",
                    "id": null,
                    "method": "session/prompt",
                    "params": {}
                },
                {
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {"value": "notification"}
                },
                {
                    "jsonrpc": "2.0",
                    "id": 99,
                    "result": {"value": "response"}
                }
            ])
        };
        let empty = serde_json::to_string(&batch(String::new())).unwrap();
        let padding_size = TARGET_SIZE - empty.len();
        let padding = format!(
            "{SENSITIVE_BASE64_CONTENT}{}",
            "A".repeat(padding_size - SENSITIVE_BASE64_CONTENT.len())
        );
        let batch = serde_json::to_string(&batch(padding)).unwrap();
        assert_eq!(batch.len(), TARGET_SIZE);

        send_client_message!(client, ClientWsMessage::Text(batch.into()));

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("server should reject an oversized mixed batch")
            .expect("connection should remain open")
            .expect("soft-limit response should be readable");
        let ClientWsMessage::Text(text) = frame else {
            panic!("expected text response, got {frame:?}");
        };
        let response: serde_json::Value = serde_json::from_str(&text).unwrap();
        let entries = response.as_array().expect("response should remain a batch");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["id"], 41);
        assert!(entries[1]["id"].is_null());
        assert!(entries.iter().all(|entry| entry["error"]["code"] == -32600));
        assert!(!text.contains(SENSITIVE_BASE64_CONTENT));
        assert!(
            timeout(Duration::from_secs(1), forwarded_rx.recv())
                .await
                .is_err(),
            "no entry from a rejected mixed batch may be forwarded"
        );

        let logs = logs_for_connection(&logs, &connection_id);
        assert!(logs.contains("Rejecting oversized JSON-RPC request batch"));
        assert!(
            !logs.contains(SENSITIVE_BASE64_CONTENT),
            "oversized batch content leaked into logs:\n{logs}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn oversized_malformed_batch_returns_null_id_errors_and_connection_survives() {
        const MAX_JSON_RPC_REQUEST_SIZE: usize = 1_024;
        const TARGET_SIZE: usize = MAX_JSON_RPC_REQUEST_SIZE + 1;

        let logs = test_logs();
        let limits = WebSocketLimits::new(4_096, 4_096, MAX_JSON_RPC_REQUEST_SIZE);
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, response) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let connection_id = response
            .headers()
            .get(HEADER_CONNECTION_ID)
            .expect("upgrade should include a connection ID")
            .to_str()
            .unwrap()
            .to_owned();
        let batch = |padding: String| {
            json!([
                {
                    "jsonrpc": "2.0",
                    "id": 70,
                    "params": {"secret": padding}
                },
                42,
                {
                    "jsonrpc": "2.0",
                    "id": 71,
                    "result": {"value": "response"},
                    "error": {"code": -1, "message": "conflicting response fields"}
                },
                {
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {}
                }
            ])
        };
        let empty = serde_json::to_string(&batch(String::new())).unwrap();
        let padding_size = TARGET_SIZE - empty.len();
        let padding = format!(
            "{SENSITIVE_TEXT_CONTENT}{}",
            "x".repeat(padding_size - SENSITIVE_TEXT_CONTENT.len())
        );
        let batch = serde_json::to_string(&batch(padding)).unwrap();
        assert_eq!(batch.len(), TARGET_SIZE);

        send_client_message!(client, ClientWsMessage::Text(batch.into()));

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("server should reject an oversized malformed batch")
            .expect("connection should remain open")
            .expect("soft-limit response should be readable");
        let ClientWsMessage::Text(text) = frame else {
            panic!("expected text response, got {frame:?}");
        };
        let response: serde_json::Value = serde_json::from_str(&text).unwrap();
        let entries = response.as_array().expect("response should remain a batch");
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry["id"].is_null()));
        assert!(entries.iter().all(|entry| entry["error"]["code"] == -32600));
        assert!(!text.contains(SENSITIVE_TEXT_CONTENT));
        assert!(
            timeout(Duration::from_secs(1), forwarded_rx.recv())
                .await
                .is_err(),
            "no entry from a rejected malformed batch may be forwarded"
        );

        let request = json!({
            "jsonrpc": "2.0",
            "id": 72,
            "method": "session/prompt",
            "params": {
                "sessionId": "session-b",
                "prompt": [{"type": "text", "text": "small"}]
            }
        });
        send_client_message!(
            client,
            ClientWsMessage::Text(serde_json::to_string(&request).unwrap().into()),
        );
        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .expect("another session should continue after the malformed batch rejection")
            .expect("agent channel should remain open");
        assert!(matches!(
            forwarded,
            RawJsonRpcMessage::Request(request) if request.id == RequestId::Number(72)
        ));

        let logs = logs_for_connection(&logs, &connection_id);
        assert!(logs.contains("Rejecting oversized JSON-RPC request batch"));
        assert!(
            !logs.contains(SENSITIVE_TEXT_CONTENT),
            "oversized malformed batch content leaked into logs:\n{logs}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn oversized_batch_error_larger_than_hard_limit_closes_with_message_too_big() {
        let requests = (0..20)
            .map(|id| json!({"jsonrpc": "2.0", "id": id, "method": "m"}))
            .collect::<Vec<_>>();
        let batch = serde_json::to_string(&requests).unwrap();
        let limits = WebSocketLimits::new(batch.len() + 64, batch.len() + 64, batch.len() - 1);
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();

        send_client_message!(client, ClientWsMessage::Text(batch.into()));

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("server should bound an oversized generated response")
            .expect("server should send a close frame")
            .expect("close frame should be readable");
        let ClientWsMessage::Close(Some(frame)) = frame else {
            panic!("expected close frame, got {frame:?}");
        };
        assert_eq!(frame.code, CloseCode::Size);
        assert!(
            timeout(Duration::from_secs(1), forwarded_rx.recv())
                .await
                .is_err(),
            "batch with an unrepresentable error must not be forwarded"
        );

        server.abort();
    }

    #[tokio::test]
    async fn oversized_dense_invalid_batch_is_bounded_and_not_forwarded() {
        let batch = format!("[{}]", vec!["0"; 4_096].join(","));
        let limits = WebSocketLimits::new(batch.len() + 64, batch.len() + 64, batch.len() - 1);
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();

        send_client_message!(client, ClientWsMessage::Text(batch.into()));

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("server should bound the dense invalid batch response")
            .expect("server should send a close frame")
            .expect("close frame should be readable");
        let ClientWsMessage::Close(Some(frame)) = frame else {
            panic!("expected close frame, got {frame:?}");
        };
        assert_eq!(frame.code, CloseCode::Size);
        assert!(
            timeout(Duration::from_secs(1), forwarded_rx.recv())
                .await
                .is_err(),
            "dense invalid batch must not be forwarded"
        );

        server.abort();
    }

    #[tokio::test]
    async fn oversized_json_rpc_request_returns_correlated_error_and_connection_survives() {
        const MAX_JSON_RPC_REQUEST_SIZE: usize = 1_024;

        let limits = WebSocketLimits::new(4_096, 4_096, MAX_JSON_RPC_REQUEST_SIZE);
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let request = text_prompt_request_with_size(1, "session-a", MAX_JSON_RPC_REQUEST_SIZE + 1);

        send_client_message!(client, ClientWsMessage::Text(request.into()));

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("server should reject an oversized JSON-RPC request")
            .expect("connection should remain open")
            .expect("soft-limit response should be readable");
        let ClientWsMessage::Text(text) = frame else {
            panic!("expected text response, got {frame:?}");
        };
        let response: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(response["id"], 1);
        assert_eq!(response["error"]["code"], -32600);
        assert_eq!(response["error"]["data"]["kind"], "payload_too_large");
        assert_eq!(
            response["error"]["data"]["max_bytes"],
            MAX_JSON_RPC_REQUEST_SIZE
        );
        assert_eq!(
            response["error"]["data"]["actual_bytes"],
            MAX_JSON_RPC_REQUEST_SIZE + 1
        );
        assert!(
            timeout(Duration::from_secs(1), forwarded_rx.recv())
                .await
                .is_err(),
            "oversized request must not be forwarded"
        );

        let request = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/prompt",
            "params": {
                "sessionId": "session-b",
                "prompt": [{"type": "text", "text": "small"}]
            }
        });
        send_client_message!(
            client,
            ClientWsMessage::Text(serde_json::to_string(&request).unwrap().into()),
        );

        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .expect("small request should be forwarded on the same connection")
            .expect("agent channel should remain open");
        assert!(matches!(
            forwarded,
            RawJsonRpcMessage::Request(request) if request.id == RequestId::Number(2)
        ));

        server.abort();
    }

    #[tokio::test]
    async fn soft_limit_error_keeps_existing_websocket_outbound_order() {
        const MAX_JSON_RPC_REQUEST_SIZE: usize = 1_024;

        let (forwarded_tx, _forwarded_rx) = mpsc::unbounded_channel();
        let registry = ConnectionRegistry::new(Arc::new(CapturingAgentFactory {
            forwarded: forwarded_tx,
        }));
        let connection_id = ConnectionRegistry::next_connection_id();
        let connection = registry
            .create_websocket_connection_with_id(connection_id.clone())
            .await;
        let mut outbound_rx = connection.subscribe_all_outbound().unwrap();
        let mut closed = connection.subscribe_closed();
        connection
            .push_all_outbound_for_test("queued-first".to_string())
            .unwrap();
        let request =
            text_prompt_request_with_size(51, "session-order", MAX_JSON_RPC_REQUEST_SIZE + 1);
        let limits = WebSocketLimits::new(4_096, 4_096, MAX_JSON_RPC_REQUEST_SIZE);
        let (mut ws_tx, mut ws_rx) = futures::channel::mpsc::unbounded::<WsMessage>();

        assert!(
            forward_client_text(
                &request,
                &mut ws_tx,
                &mut outbound_rx,
                &mut closed,
                &connection_id,
                &connection,
                Some(limits),
            )
            .await
        );
        drain_queued_outbound(&mut ws_tx, &mut outbound_rx, &connection_id).await;

        let WsMessage::Text(first) = ws_rx.next().await.unwrap() else {
            panic!("expected queued text frame");
        };
        assert_eq!(first.as_str(), "queued-first");
        let WsMessage::Text(second) = ws_rx.next().await.unwrap() else {
            panic!("expected soft-limit error frame");
        };
        let response: serde_json::Value = serde_json::from_str(second.as_str()).unwrap();
        assert_eq!(response["id"], 51);
        assert_eq!(response["error"]["code"], -32600);

        registry.remove(&connection_id).await;
        connection.shutdown().await;
    }

    #[tokio::test]
    async fn oversized_null_id_request_returns_correlated_error_and_connection_survives() {
        const MAX_JSON_RPC_REQUEST_SIZE: usize = 1_024;

        let limits = WebSocketLimits::new(4_096, 4_096, MAX_JSON_RPC_REQUEST_SIZE);
        let logs = test_logs();
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, response) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let connection_id = response
            .headers()
            .get(HEADER_CONNECTION_ID)
            .expect("upgrade should include a connection ID")
            .to_str()
            .unwrap()
            .to_owned();
        let request =
            null_id_text_prompt_request_with_size("session-null", MAX_JSON_RPC_REQUEST_SIZE + 1);

        send_client_message!(client, ClientWsMessage::Text(request.into()));

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("server should reject an oversized null-ID request")
            .expect("connection should remain open")
            .expect("soft-limit response should be readable");
        let ClientWsMessage::Text(text) = frame else {
            panic!("expected text response, got {frame:?}");
        };
        let response: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(response["id"].is_null());
        assert_eq!(response["error"]["code"], -32600);
        assert_eq!(response["error"]["data"]["kind"], "payload_too_large");
        assert_eq!(
            response["error"]["data"]["max_bytes"],
            MAX_JSON_RPC_REQUEST_SIZE
        );
        assert_eq!(
            response["error"]["data"]["actual_bytes"],
            MAX_JSON_RPC_REQUEST_SIZE + 1
        );
        assert!(
            timeout(Duration::from_secs(1), forwarded_rx.recv())
                .await
                .is_err(),
            "oversized null-ID request must not be forwarded"
        );

        let request = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "session/prompt",
            "params": {
                "sessionId": "session-b",
                "prompt": [{"type": "text", "text": "small"}]
            }
        });
        send_client_message!(
            client,
            ClientWsMessage::Text(serde_json::to_string(&request).unwrap().into()),
        );

        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .expect("small request should be forwarded on the same connection")
            .expect("agent channel should remain open");
        assert!(matches!(
            forwarded,
            RawJsonRpcMessage::Request(request) if request.id == RequestId::Number(10)
        ));

        let logs = logs_for_connection(&logs, &connection_id);
        assert!(logs.contains("Rejecting oversized JSON-RPC request"));
        assert!(logs.contains("actual_bytes=1025"));
        assert!(logs.contains("max_bytes=1024"));
        assert!(logs.contains("error_category=\"payload_too_large\""));
        assert!(
            !logs.contains(SENSITIVE_TEXT_CONTENT),
            "oversized request content leaked into logs:\n{logs}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn json_rpc_request_at_soft_limit_is_forwarded() {
        const MAX_JSON_RPC_REQUEST_SIZE: usize = 1_024;

        let limits = WebSocketLimits::new(4_096, 4_096, MAX_JSON_RPC_REQUEST_SIZE);
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let request =
            text_prompt_request_with_size(3, "session-boundary", MAX_JSON_RPC_REQUEST_SIZE);

        send_client_message!(client, ClientWsMessage::Text(request.into()));

        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .expect("request at the soft limit should be forwarded")
            .expect("agent channel should remain open");
        assert!(matches!(
            forwarded,
            RawJsonRpcMessage::Request(request) if request.id == RequestId::Number(3)
        ));
        assert!(
            timeout(Duration::from_secs(1), client.next())
                .await
                .is_err(),
            "request at the soft limit must not receive a payload-too-large error"
        );

        server.abort();
    }

    #[tokio::test]
    async fn json_rpc_request_below_soft_limit_is_forwarded_without_logging_content() {
        const MAX_JSON_RPC_REQUEST_SIZE: usize = 1_024;

        let logs = test_logs();
        let limits = WebSocketLimits::new(4_096, 4_096, MAX_JSON_RPC_REQUEST_SIZE);
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, response) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let connection_id = response
            .headers()
            .get(HEADER_CONNECTION_ID)
            .expect("upgrade should include a connection ID")
            .to_str()
            .unwrap()
            .to_owned();
        let request =
            text_prompt_request_with_size(5, "session-below", MAX_JSON_RPC_REQUEST_SIZE - 1);

        send_client_message!(client, ClientWsMessage::Text(request.into()));

        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .expect("request below the soft limit should be forwarded")
            .expect("agent channel should remain open");
        assert!(matches!(
            forwarded,
            RawJsonRpcMessage::Request(request) if request.id == RequestId::Number(5)
        ));
        assert!(
            timeout(Duration::from_secs(1), client.next())
                .await
                .is_err(),
            "request below the soft limit must not receive a payload-too-large error"
        );

        let connection_logs = logs_for_connection(&logs, &connection_id);
        assert!(connection_logs.contains("Client → Agent: 1023 bytes"));
        let logs = all_test_logs(&logs);
        assert!(
            !logs.contains(SENSITIVE_TEXT_CONTENT),
            "accepted request content leaked into logs:\n{logs}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn accepted_string_request_id_is_not_logged() {
        let logs = test_logs();
        let limits = WebSocketLimits::new(4_096, 4_096, 2_048);
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, response) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let connection_id = response
            .headers()
            .get(HEADER_CONNECTION_ID)
            .expect("upgrade should include a connection ID")
            .to_str()
            .unwrap()
            .to_owned();
        let request = json!({
            "jsonrpc": "2.0",
            "id": SENSITIVE_REQUEST_ID_CONTENT,
            "method": "session/prompt",
            "params": {
                "sessionId": "session-sensitive-id",
                "prompt": [{"type": "text", "text": "small"}]
            }
        });

        send_client_message!(
            client,
            ClientWsMessage::Text(serde_json::to_string(&request).unwrap().into()),
        );
        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .expect("request should reach the agent")
            .expect("agent channel should remain open");
        assert!(matches!(
            forwarded,
            RawJsonRpcMessage::Request(request)
                if request.id == RequestId::Str(SENSITIVE_REQUEST_ID_CONTENT.to_string())
        ));

        let logs = logs_for_connection(&logs, &connection_id);
        assert!(
            !logs.contains(SENSITIVE_REQUEST_ID_CONTENT),
            "request ID leaked into logs:\n{logs}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn real_protocol_actor_does_not_log_accepted_invalid_or_malformed_content() {
        let logs = test_logs();
        let app = AcpHttpServer::new(agent_client_protocol_test::testy::Testy::new)
            .with_websocket_limits(WebSocketLimits::new(4_096, 4_096, 2_048))
            .into_router();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let request = json!({
            "jsonrpc": "2.0",
            "id": SENSITIVE_REQUEST_ID_CONTENT,
            "method": "session/prompt",
            "params": {
                "sessionId": "missing-sensitive-session",
                "prompt": [
                    {"type": "text", "text": SENSITIVE_TEXT_CONTENT},
                    {
                        "type": "image",
                        "data": SENSITIVE_BASE64_CONTENT,
                        "mimeType": "image/png"
                    }
                ]
            }
        });

        send_client_message!(
            client,
            ClientWsMessage::Text(serde_json::to_string(&request).unwrap().into()),
        );
        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("real protocol actor should answer the request")
            .expect("connection should remain open")
            .expect("response should be readable");
        assert!(matches!(frame, ClientWsMessage::Text(_)));

        let typed_invalid_request = json!({
            "jsonrpc": "2.0",
            "id": "typed-invalid-sensitive-request",
            "method": "session/prompt",
            "params": {
                "sessionId": {"invalid": true},
                "prompt": [
                    {"type": "text", "text": SENSITIVE_TYPED_INVALID_TEXT_CONTENT},
                    {
                        "type": "image",
                        "data": SENSITIVE_TYPED_INVALID_BASE64_CONTENT,
                        "mimeType": "image/png"
                    }
                ]
            }
        });
        send_client_message!(
            client,
            ClientWsMessage::Text(
                serde_json::to_string(&typed_invalid_request)
                    .unwrap()
                    .into(),
            ),
        );
        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("real protocol actor should reject typed-invalid params")
            .expect("connection should remain open")
            .expect("invalid-params response should be readable");
        let ClientWsMessage::Text(typed_invalid_response) = frame else {
            panic!("expected text frame, got {frame:?}");
        };
        let typed_invalid_json: serde_json::Value =
            serde_json::from_str(&typed_invalid_response).unwrap();
        assert_eq!(typed_invalid_json["id"], "typed-invalid-sensitive-request");
        assert_eq!(typed_invalid_json["error"]["code"], -32602);
        assert!(
            !typed_invalid_response.contains(SENSITIVE_TYPED_INVALID_TEXT_CONTENT),
            "typed-invalid prompt leaked into the JSON-RPC error: {typed_invalid_response}"
        );
        assert!(
            !typed_invalid_response.contains(SENSITIVE_TYPED_INVALID_BASE64_CONTENT),
            "typed-invalid base64 leaked into the JSON-RPC error: {typed_invalid_response}"
        );

        send_client_message!(
            client,
            ClientWsMessage::Text(format!("{{not json {SENSITIVE_MALFORMED_CONTENT}").into()),
        );
        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("real protocol actor should answer malformed input")
            .expect("connection should remain open")
            .expect("parse-error response should be readable");
        assert!(matches!(frame, ClientWsMessage::Text(_)));

        let logs = all_test_logs(&logs);
        let sensitive_logs = logs
            .lines()
            .filter(|line| {
                line.contains(SENSITIVE_TEXT_CONTENT)
                    || line.contains(SENSITIVE_BASE64_CONTENT)
                    || line.contains(SENSITIVE_MALFORMED_CONTENT)
                    || line.contains(SENSITIVE_TYPED_INVALID_TEXT_CONTENT)
                    || line.contains(SENSITIVE_TYPED_INVALID_BASE64_CONTENT)
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !logs.contains(SENSITIVE_TEXT_CONTENT),
            "accepted prompt leaked into logs:\n{sensitive_logs}"
        );
        assert!(
            !logs.contains(SENSITIVE_BASE64_CONTENT),
            "accepted base64 content leaked into logs:\n{sensitive_logs}"
        );
        assert!(
            !logs.contains(SENSITIVE_MALFORMED_CONTENT),
            "malformed content leaked into logs:\n{sensitive_logs}"
        );
        assert!(
            !logs.contains(SENSITIVE_TYPED_INVALID_TEXT_CONTENT),
            "typed-invalid prompt leaked into logs:\n{sensitive_logs}"
        );
        assert!(
            !logs.contains(SENSITIVE_TYPED_INVALID_BASE64_CONTENT),
            "typed-invalid base64 leaked into logs:\n{sensitive_logs}"
        );
        let incoming_logs = logs
            .lines()
            .filter(|line| line.contains("agent_client_protocol::jsonrpc::incoming_actor"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            incoming_logs.contains("transport_single"),
            "real incoming actor trace was not captured:\n{logs}"
        );
        assert!(
            !logs.contains(SENSITIVE_REQUEST_ID_CONTENT),
            "request ID leaked into core or HTTP logs:\n{logs}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn real_protocol_actor_does_not_log_typed_response_content() {
        let logs = test_logs();
        let response_processed = Arc::new(tokio::sync::Notify::new());
        let agent_response_processed = response_processed.clone();
        let app = AcpHttpServer::new(move || {
            let response_processed = agent_response_processed.clone();
            Agent
                .builder()
                .name("typed-response-log-test-agent")
                .on_receive_dispatch(
                    async |dispatch: Dispatch<ReadTextFileRequest, UntypedMessage>, _connection| {
                        match dispatch {
                            Dispatch::Response(result, router) => router.route_with_result(result),
                            Dispatch::Request(..) | Dispatch::Notification(..) => Ok(()),
                        }
                    },
                    agent_client_protocol::on_receive_dispatch!(),
                )
                .with_spawned(move |connection| {
                    let response_processed = response_processed.clone();
                    async move {
                        connection
                            .send_request(ReadTextFileRequest::new(
                                SessionId::new("typed-response-session"),
                                PathBuf::from("/tmp/typed-response.txt"),
                            ))
                            .block_task()
                            .await?;
                        response_processed.notify_one();
                        Ok(())
                    }
                })
        })
        .with_websocket_limits(WebSocketLimits::new(4_096, 4_096, 2_048))
        .into_router();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("real protocol actor should send the typed request")
            .expect("connection should remain open")
            .expect("typed request should be readable");
        let ClientWsMessage::Text(request) = frame else {
            panic!("expected text frame, got {frame:?}");
        };
        let request: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(request["method"], "fs/read_text_file");
        let callback_response = json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": serde_json::to_value(ReadTextFileResponse::new(
                SENSITIVE_TYPED_RESPONSE_CONTENT
            )).unwrap()
        });
        send_client_message!(
            client,
            ClientWsMessage::Text(serde_json::to_string(&callback_response).unwrap().into()),
        );
        timeout(Duration::from_secs(1), response_processed.notified())
            .await
            .expect("typed response should be parsed and routed");

        let logs = all_test_logs(&logs);
        let response_parse_logs = logs
            .lines()
            .filter(|line| line.contains("agent_client_protocol::jsonrpc:"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            response_parse_logs.contains("parse ok"),
            "real typed-response parser trace was not captured:\n{logs}"
        );
        assert!(
            !response_parse_logs.contains(SENSITIVE_TYPED_RESPONSE_CONTENT),
            "typed response content leaked into parser logs:\n{response_parse_logs}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn base64_style_request_above_soft_limit_returns_correlated_error_without_logging_content()
     {
        const MAX_JSON_RPC_REQUEST_SIZE: usize = 1_024;

        let logs = test_logs();
        let limits = WebSocketLimits::new(4_096, 4_096, MAX_JSON_RPC_REQUEST_SIZE);
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, response) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let connection_id = response
            .headers()
            .get(HEADER_CONNECTION_ID)
            .expect("upgrade should include a connection ID")
            .to_str()
            .unwrap()
            .to_owned();
        let request =
            base64_prompt_request_with_size(4, "session-image", MAX_JSON_RPC_REQUEST_SIZE + 1);

        send_client_message!(client, ClientWsMessage::Text(request.into()));

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("server should reject an oversized base64-style request")
            .expect("connection should remain open")
            .expect("soft-limit response should be readable");
        let ClientWsMessage::Text(text) = frame else {
            panic!("expected text response, got {frame:?}");
        };
        let response: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(response["id"], 4);
        assert_eq!(response["error"]["code"], -32600);
        assert_eq!(response["error"]["data"]["kind"], "payload_too_large");
        assert_eq!(
            response["error"]["data"]["max_bytes"],
            MAX_JSON_RPC_REQUEST_SIZE
        );
        assert_eq!(
            response["error"]["data"]["actual_bytes"],
            MAX_JSON_RPC_REQUEST_SIZE + 1
        );
        assert!(
            timeout(Duration::from_secs(1), forwarded_rx.recv())
                .await
                .is_err(),
            "oversized base64-style request must not be forwarded"
        );

        let logs = logs_for_connection(&logs, &connection_id);
        assert!(logs.contains("Rejecting oversized JSON-RPC request"));
        assert!(logs.contains("actual_bytes=1025"));
        assert!(logs.contains("max_bytes=1024"));
        assert!(logs.contains("error_category=\"payload_too_large\""));
        assert!(
            !logs.contains(SENSITIVE_BASE64_CONTENT),
            "oversized base64 content leaked into logs:\n{logs}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn soft_limit_does_not_misrepresent_notifications_or_responses_as_requests() {
        const MAX_JSON_RPC_REQUEST_SIZE: usize = 1_024;
        const OVERSIZED: usize = MAX_JSON_RPC_REQUEST_SIZE + 1;

        let limits = WebSocketLimits::new(4_096, 4_096, MAX_JSON_RPC_REQUEST_SIZE);
        let (addr, _registry, mut forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();

        let notification = |text: String| {
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"text": text}
            })
        };
        let empty = serde_json::to_string(&notification(String::new())).unwrap();
        let notification =
            serde_json::to_string(&notification("n".repeat(OVERSIZED - empty.len()))).unwrap();
        assert_eq!(notification.len(), OVERSIZED);
        send_client_message!(client, ClientWsMessage::Text(notification.into()));
        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .expect("oversized notification should reach the agent")
            .expect("agent channel should remain open");
        assert!(matches!(forwarded, RawJsonRpcMessage::Notification(_)));
        assert!(
            timeout(Duration::from_secs(1), client.next())
                .await
                .is_err(),
            "notification must not receive a JSON-RPC response"
        );

        let response = |data: String| {
            json!({
                "jsonrpc": "2.0",
                "id": 8,
                "result": {"data": data}
            })
        };
        let empty = serde_json::to_string(&response(String::new())).unwrap();
        let response =
            serde_json::to_string(&response("r".repeat(OVERSIZED - empty.len()))).unwrap();
        assert_eq!(response.len(), OVERSIZED);
        send_client_message!(client, ClientWsMessage::Text(response.into()));
        let forwarded = timeout(Duration::from_secs(1), forwarded_rx.recv())
            .await
            .expect("oversized response should reach the agent")
            .expect("agent channel should remain open");
        assert!(matches!(forwarded, RawJsonRpcMessage::Response(_)));
        assert!(
            timeout(Duration::from_secs(1), client.next())
                .await
                .is_err(),
            "response must not receive a payload-too-large error"
        );

        server.abort();
    }

    #[tokio::test]
    async fn oversized_ws_message_closes_with_message_too_big() {
        const MAX_MESSAGE_SIZE: usize = 1_024;

        drop(test_logs());
        let limits = WebSocketLimits::new(2_048, MAX_MESSAGE_SIZE, 512);
        let app = AcpHttpServer::new(agent_client_protocol_test::testy::Testy::new)
            .with_websocket_limits(limits)
            .into_router();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let request = text_prompt_request_with_size(1, "session-a", MAX_MESSAGE_SIZE + 1);

        send_client_message!(client, ClientWsMessage::Text(request.into()));

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("server should respond to an oversized message")
            .expect("server should send a close frame")
            .expect("close frame should be readable");
        let ClientWsMessage::Close(Some(frame)) = frame else {
            panic!("expected close frame, got {frame:?}");
        };
        assert_eq!(frame.code, CloseCode::Size);

        server.abort();
    }

    #[tokio::test]
    async fn oversized_ws_frame_closes_with_message_too_big() {
        const MAX_FRAME_SIZE: usize = 1_024;

        let limits = WebSocketLimits::new(MAX_FRAME_SIZE, 2_048, 512);
        let (addr, _registry, _forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let request = text_prompt_request_with_size(6, "session-frame", MAX_FRAME_SIZE + 1);

        send_client_message!(client, ClientWsMessage::Text(request.into()));

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("server should respond to an oversized frame")
            .expect("server should send a close frame")
            .expect("close frame should be readable");
        let ClientWsMessage::Close(Some(frame)) = frame else {
            panic!("expected close frame, got {frame:?}");
        };
        assert_eq!(frame.code, CloseCode::Size);

        server.abort();
    }

    #[tokio::test]
    async fn hard_limit_close_cleans_up_connection() {
        const MAX_MESSAGE_SIZE: usize = 1_024;

        let limits = WebSocketLimits::new(2_048, MAX_MESSAGE_SIZE, 512);
        let (addr, registry, _forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, response) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let connection_id = response
            .headers()
            .get(HEADER_CONNECTION_ID)
            .expect("upgrade should include a connection ID")
            .to_str()
            .unwrap()
            .to_owned();
        let connection = timeout(Duration::from_secs(1), async {
            loop {
                if let Some(connection) = registry.get(&connection_id).await {
                    break connection;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("WebSocket connection should be registered");
        let closed = connection.subscribe_closed();
        let request = text_prompt_request_with_size(7, "session-cleanup", MAX_MESSAGE_SIZE + 1);

        send_client_message!(client, ClientWsMessage::Text(request.into()));

        let frame = timeout(Duration::from_secs(1), client.next())
            .await
            .expect("server should respond to an oversized message")
            .expect("server should send a close frame")
            .expect("close frame should be readable");
        let ClientWsMessage::Close(Some(frame)) = frame else {
            panic!("expected close frame, got {frame:?}");
        };
        assert_eq!(frame.code, CloseCode::Size);

        timeout(Duration::from_secs(1), async {
            while registry.len().await != 0 || !*closed.borrow() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("hard close should remove and shut down the connection");

        server.abort();
    }

    #[tokio::test]
    async fn abrupt_websocket_disconnect_uses_generic_error_path_and_cleans_up() {
        let logs = test_logs();
        let limits = WebSocketLimits::new(4_096, 4_096, 1_024);
        let (addr, registry, _forwarded_rx, server) = spawn_capturing_server(limits).await;
        let (mut client, response) = connect_async(format!("ws://{addr}/acp")).await.unwrap();
        let connection_id = response
            .headers()
            .get(HEADER_CONNECTION_ID)
            .expect("upgrade should include a connection ID")
            .to_str()
            .unwrap()
            .to_owned();
        let connection = timeout(Duration::from_secs(1), async {
            loop {
                if let Some(connection) = registry.get(&connection_id).await {
                    break connection;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("WebSocket connection should be registered");
        let closed = connection.subscribe_closed();

        futures::AsyncWriteExt::close(client.get_mut())
            .await
            .unwrap();
        drop(client);

        timeout(Duration::from_secs(1), async {
            while registry.len().await != 0 || !*closed.borrow() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("generic WebSocket error should clean up the connection");

        let logs = logs_for_connection(&logs, &connection_id);
        assert!(
            logs.contains("WebSocket error:"),
            "generic error log missing:\n{logs}"
        );
        assert!(!logs.contains("message_too_long"));
        assert!(!logs.contains("WebSocket close 1009"));

        server.abort();
    }

    #[test]
    fn non_capacity_websocket_error_is_not_message_too_long() {
        let error = axum::Error::new(tungstenite::Error::Protocol(
            tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
        ));

        assert_eq!(message_too_long(&error), None);
    }
}
