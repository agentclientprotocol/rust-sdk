use std::sync::Arc;

use agent_client_protocol::{Error as AcpError, RawJsonRpcMessage, schema::v1::RequestId};
use axum::{
    extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    http::HeaderValue,
    response::Response,
};
use futures::{SinkExt, StreamExt};
use tracing::{debug, error, info, trace, warn};

use crate::{
    connection::ConnectionRegistry,
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
    let (replay, mut outbound_rx) = connection.subscribe_all_outbound().await;
    let mut closed = connection.subscribe_closed();

    debug!(connection_id = %connection_id, "Starting WebSocket message loop");

    for text in replay {
        trace!(connection_id = %connection_id, payload = %text, "Agent → Client (replay): {} bytes", text.len());
        if ws_tx.send(WsMessage::Text(text.into())).await.is_err() {
            error!(connection_id = %connection_id, "WebSocket send failed during replay");
            if let Some(conn) = registry.remove(&connection_id).await {
                conn.shutdown().await;
            }
            return;
        }
    }

    loop {
        if *closed.borrow() {
            break;
        }
        tokio::select! {
            msg_result = ws_rx.next() => {
                match msg_result {
                    Some(Ok(WsMessage::Text(text))) => {
                        let text_str = text.to_string();
                        trace!(connection_id = %connection_id, payload = %text_str, "Client → Agent: {} bytes", text_str.len());
                        match serde_json::from_str::<RawJsonRpcMessage>(&text_str) {
                            Ok(parsed) => {
                                if let Some(sid) = session_id_from_message(&parsed)
                                    && let RawJsonRpcMessage::Request(req) = &parsed {
                                        trace!(connection_id = %connection_id, session_id = %sid, request_id = ?req.id, "Client → Agent (session)");
                                    }
                                if connection.send_to_agent(parsed).is_err() {
                                    error!(connection_id = %connection_id, "Agent channel closed");
                                    break;
                                }
                            }
                            Err(e) => {
                                let message = format!("malformed JSON-RPC payload: {e}");
                                warn!(connection_id = %connection_id, "Returning parse error for malformed JSON-RPC frame: {e}");
                                let response = RawJsonRpcMessage::response(
                                    RequestId::Null,
                                    Err(AcpError::parse_error().data(message)),
                                );
                                let text = match serde_json::to_string(&response) {
                                    Ok(text) => text,
                                    Err(e) => {
                                        error!(connection_id = %connection_id, "Failed to serialize parse error response: {e}");
                                        break;
                                    }
                                };
                                if ws_tx.send(WsMessage::Text(text.into())).await.is_err() {
                                    error!(connection_id = %connection_id, "WebSocket send failed");
                                    break;
                                }
                            }
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

            recv = outbound_rx.recv() => {
                match recv {
                    Some(text) => {
                        trace!(connection_id = %connection_id, payload = %text, "Agent → Client: {} bytes", text.len());
                        if ws_tx.send(WsMessage::Text(text.into())).await.is_err() {
                            error!(connection_id = %connection_id, "WebSocket send failed");
                            break;
                        }
                    }
                    None => break,
                }
            }

            changed = closed.changed() => {
                if changed.is_err() || *closed.borrow() {
                    break;
                }
            }
        }
    }

    debug!(connection_id = %connection_id, "Cleaning up WebSocket connection");
    if let Some(conn) = registry.remove(&connection_id).await {
        conn.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::{
        Channel,
        schema::v1::{RequestId, Response as RpcResponse},
    };
    use async_tungstenite::{tokio::connect_async, tungstenite::Message as ClientWsMessage};
    use axum::{Router, extract::WebSocketUpgrade, routing::get};
    use futures::{StreamExt as _, future::BoxFuture};
    use serde_json::json;
    use tokio::{
        net::TcpListener,
        sync::mpsc,
        time::{Duration, timeout},
    };

    use crate::connection::{AgentFactory, ConnectionRegistry};

    use super::*;

    struct CapturingAgentFactory {
        forwarded: mpsc::UnboundedSender<RawJsonRpcMessage>,
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
                    tx: _,
                } = agent;
                while let Some(message) = incoming.next().await {
                    if forwarded.send(message?).is_err() {
                        break;
                    }
                }
                Ok(())
            });

            (transport, future)
        }
    }

    #[tokio::test]
    async fn malformed_ws_frame_returns_parse_error_response_and_continues() {
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
                    async move { handle_ws_upgrade(registry, ws) }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut client, _) = connect_async(format!("ws://{addr}/acp")).await.unwrap();

        client
            .send(ClientWsMessage::Text("{not json".into()))
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
        assert!(
            value["error"]["data"]
                .as_str()
                .unwrap()
                .contains("malformed JSON-RPC payload")
        );

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

        server.abort();
    }
}
