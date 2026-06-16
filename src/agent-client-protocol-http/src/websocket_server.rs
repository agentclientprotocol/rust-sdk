use std::sync::Arc;

use agent_client_protocol::RawJsonRpcMessage;
use axum::{
    extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    http::HeaderValue,
    response::Response,
};
use futures::{SinkExt, StreamExt};
use tracing::{debug, error, info, trace, warn};

use crate::{
    connection::{ConnectionRegistry, ResponseRoute},
    protocol::{HEADER_CONNECTION_ID, session_id_from_message},
};

pub(crate) async fn handle_ws_upgrade(
    registry: Arc<ConnectionRegistry>,
    ws: WebSocketUpgrade,
) -> Response {
    let (connection_id, connection) = registry.create_connection().await;

    connection.start_router().await;

    let conn_id_for_handler = connection_id.clone();
    let registry_for_handler = registry.clone();
    let mut response = ws.on_upgrade(move |socket| async move {
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
    info!(connection_id = %connection_id, "WebSocket connection created");
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
                                if let Some(sid) = session_id_from_message(&parsed) {
                                    connection.ensure_session(&sid).await;
                                    if let RawJsonRpcMessage::Request(req) = &parsed {
                                        connection
                                            .record_pending_route(
                                                req.id.clone(),
                                                ResponseRoute::Session(sid),
                                            )
                                            .await;
                                    }
                                }
                                if connection.send_to_agent(parsed).is_err() {
                                    error!(connection_id = %connection_id, "Agent channel closed");
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!(connection_id = %connection_id, "Ignoring malformed JSON-RPC frame: {e}");
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
                    Ok(text) => {
                        trace!(connection_id = %connection_id, payload = %text, "Agent → Client: {} bytes", text.len());
                        if ws_tx.send(WsMessage::Text(text.into())).await.is_err() {
                            error!(connection_id = %connection_id, "WebSocket send failed");
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(connection_id = %connection_id, "WebSocket lagged {n} messages");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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
