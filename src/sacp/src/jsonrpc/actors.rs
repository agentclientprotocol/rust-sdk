use std::collections::HashMap;
use std::pin::pin;

// Types re-exported from crate root
use futures::AsyncBufReadExt as _;
use futures::AsyncRead;
use futures::AsyncWrite;
use futures::AsyncWriteExt as _;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::io::BufReader;
use uuid::Uuid;

use crate::MessageAndCx;
use crate::UntypedMessage;
use crate::jsonrpc::JrConnectionCx;
use crate::jsonrpc::JrMessageHandler;
use crate::jsonrpc::JrRequestCx;
use crate::jsonrpc::OutgoingMessage;
use crate::jsonrpc::ReplyMessage;

use super::Handled;

/// The "reply actor" manages a queue of pending replies.
pub(super) async fn reply_actor(
    mut reply_rx: mpsc::UnboundedReceiver<ReplyMessage>,
) -> Result<(), crate::Error> {
    // Map from the `id` to a oneshot sender where we should send the value.
    let mut map = HashMap::new();

    while let Some(message) = reply_rx.next().await {
        match message {
            ReplyMessage::Subscribe(id, message_tx) => {
                // total hack: id's don't implement Eq
                tracing::trace!(?id, "reply_actor: subscribing to response");
                let id = serde_json::to_value(&id).unwrap();
                map.insert(id, message_tx);
            }
            ReplyMessage::Dispatch(id, value) => {
                let id_debug = &id;
                let is_ok = value.is_ok();
                tracing::trace!(?id_debug, is_ok, "reply_actor: dispatching response");
                let id = serde_json::to_value(&id).unwrap();
                if let Some(message_tx) = map.remove(&id) {
                    // If the receiver is no longer interested in the reply,
                    // that's ok with us.
                    let result = message_tx.send(value);
                    if result.is_err() {
                        tracing::warn!(
                            ?id,
                            "reply_actor: failed to send response, receiver dropped"
                        );
                    } else {
                        tracing::trace!(
                            ?id,
                            "reply_actor: successfully dispatched response to receiver"
                        );
                    }
                } else {
                    tracing::warn!(
                        ?id,
                        "reply_actor: received response for unknown id, no subscriber found"
                    );
                }
            }
        }
    }
    Ok(())
}

// ============================================================================
// Split Actors for Pluggable Transport
// ============================================================================

/// Outgoing protocol actor: Converts application-level OutgoingMessage to protocol-level jsonrpcmsg::Message.
///
/// This actor handles JSON-RPC protocol semantics:
/// - Assigns unique IDs to outgoing requests
/// - Subscribes to reply_actor for response correlation
/// - Converts OutgoingMessage variants to jsonrpcmsg::Message
///
/// This is the protocol layer - it has no knowledge of how messages are transported.
pub(super) async fn outgoing_protocol_actor(
    mut outgoing_rx: mpsc::UnboundedReceiver<OutgoingMessage>,
    reply_tx: mpsc::UnboundedSender<ReplyMessage>,
    transport_tx: mpsc::UnboundedSender<Result<jsonrpcmsg::Message, crate::Error>>,
) -> Result<(), crate::Error> {
    while let Some(message) = outgoing_rx.next().await {
        tracing::debug!(?message, "outgoing_protocol_actor");

        // Create the message to be sent over the transport
        let json_rpc_message = match message {
            OutgoingMessage::Request {
                method,
                params,
                response_tx: response_rx,
            } => {
                // Generate a fresh UUID to use for the request id
                let uuid = Uuid::new_v4();
                let id = jsonrpcmsg::Id::String(uuid.to_string());

                // Record where the reply should be sent once it arrives.
                reply_tx
                    .unbounded_send(ReplyMessage::Subscribe(id.clone(), response_rx))
                    .map_err(crate::Error::into_internal_error)?;

                jsonrpcmsg::Message::Request(jsonrpcmsg::Request::new_v2(method, params, Some(id)))
            }
            OutgoingMessage::Notification { method, params } => {
                jsonrpcmsg::Message::Request(jsonrpcmsg::Request::new_v2(method, params, None))
            }
            OutgoingMessage::Response {
                id,
                response: Ok(value),
            } => {
                tracing::debug!(?id, "Sending success response");
                jsonrpcmsg::Message::Response(jsonrpcmsg::Response::success_v2(value, Some(id)))
            }
            OutgoingMessage::Response {
                id,
                response: Err(error),
            } => {
                tracing::warn!(?id, ?error, "Sending error response");
                // Convert crate::Error to jsonrpcmsg::Error
                let jsonrpc_error = jsonrpcmsg::Error {
                    code: error.code,
                    message: error.message,
                    data: error.data,
                };
                jsonrpcmsg::Message::Response(jsonrpcmsg::Response::error_v2(
                    jsonrpc_error,
                    Some(id),
                ))
            }
            OutgoingMessage::Error { error } => {
                // Convert crate::Error to jsonrpcmsg::Error
                let jsonrpc_error = jsonrpcmsg::Error {
                    code: error.code,
                    message: error.message,
                    data: error.data,
                };
                // Response with id: None means this is an error notification that couldn't be
                // correlated to a specific request (e.g., parse error before we could read the id)
                jsonrpcmsg::Message::Response(jsonrpcmsg::Response::error_v2(jsonrpc_error, None))
            }
        };

        // Send to transport layer (wrapped in Ok since transport expects Result)
        transport_tx
            .unbounded_send(Ok(json_rpc_message))
            .map_err(crate::Error::into_internal_error)?;
    }
    Ok(())
}

/// Transport outgoing actor: Serializes jsonrpcmsg::Message and writes to byte stream.
///
/// This actor handles transport mechanics:
/// - Unwraps Result<Message> from the channel
/// - Serializes jsonrpcmsg::Message to JSON
/// - Writes newline-delimited JSON to the stream
/// - Handles serialization errors
///
/// This is the transport layer - it has no knowledge of protocol semantics (IDs, correlation, etc.).
pub(super) async fn transport_outgoing_actor(
    mut transport_rx: mpsc::UnboundedReceiver<Result<jsonrpcmsg::Message, crate::Error>>,
    outgoing_bytes: impl AsyncWrite,
) -> Result<(), crate::Error> {
    let mut outgoing_bytes = pin!(outgoing_bytes);

    while let Some(message_result) = transport_rx.next().await {
        // Unwrap the Result - errors here would be from the channel itself
        let json_rpc_message = message_result?;
        match serde_json::to_vec(&json_rpc_message) {
            Ok(mut bytes) => {
                if let Ok(msg_str) = std::str::from_utf8(&bytes) {
                    tracing::trace!(message = %msg_str, "Sending JSON-RPC message");
                }
                bytes.push('\n' as u8);
                outgoing_bytes
                    .write_all(&bytes)
                    .await
                    .map_err(crate::Error::into_internal_error)?;
            }

            Err(serialization_error) => {
                match json_rpc_message {
                    jsonrpcmsg::Message::Request(_request) => {
                        // If we failed to serialize a request,
                        // just ignore it.
                        //
                        // Q: (Maybe it'd be nice to "reply" with an error?)
                        tracing::error!(
                            ?serialization_error,
                            "Failed to serialize request, ignoring"
                        );
                    }
                    jsonrpcmsg::Message::Response(response) => {
                        // If we failed to serialize a *response*,
                        // send an error in response.
                        tracing::error!(?serialization_error, id = ?response.id, "Failed to serialize response, sending internal_error instead");
                        // Convert crate::Error to jsonrpcmsg::Error
                        let acp_error = crate::Error::internal_error();
                        let jsonrpc_error = jsonrpcmsg::Error {
                            code: acp_error.code,
                            message: acp_error.message,
                            data: acp_error.data,
                        };
                        outgoing_bytes
                            .write_all(
                                &serde_json::to_vec(&jsonrpcmsg::Response::error(
                                    jsonrpc_error,
                                    response.id,
                                ))
                                .unwrap(),
                            )
                            .await
                            .map_err(crate::Error::into_internal_error)?;
                    }
                }
            }
        };
    }
    Ok(())
}

/// Incoming protocol actor: Routes jsonrpcmsg::Message to reply_actor or handler.
///
/// This actor handles JSON-RPC protocol semantics:
/// - Routes responses to reply_actor (for request/response correlation)
/// - Routes requests/notifications to handler chain
/// - Converts jsonrpcmsg::Request to UntypedMessage for handlers
///
/// This is the protocol layer - it has no knowledge of how messages arrived.
pub(super) async fn incoming_protocol_actor(
    json_rpc_cx: &JrConnectionCx,
    mut transport_rx: mpsc::UnboundedReceiver<Result<jsonrpcmsg::Message, crate::Error>>,
    reply_tx: mpsc::UnboundedSender<ReplyMessage>,
    mut handler: impl JrMessageHandler,
) -> Result<(), crate::Error> {
    while let Some(message_result) = transport_rx.next().await {
        match message_result {
            Ok(message) => match message {
                jsonrpcmsg::Message::Request(request) => {
                    tracing::trace!(method = %request.method, id = ?request.id, "Handling request");
                    dispatch_request(json_rpc_cx, request, &mut handler).await?
                }
                jsonrpcmsg::Message::Response(response) => {
                    tracing::trace!(id = ?response.id, has_result = response.result.is_some(), has_error = response.error.is_some(), "Handling response");
                    if let Some(id) = response.id {
                        if let Some(value) = response.result {
                            reply_tx
                                .unbounded_send(ReplyMessage::Dispatch(id, Ok(value)))
                                .map_err(crate::Error::into_internal_error)?;
                        } else if let Some(error) = response.error {
                            // Convert jsonrpcmsg::Error to crate::Error
                            let acp_error = crate::Error {
                                code: error.code,
                                message: error.message,
                                data: error.data,
                            };
                            reply_tx
                                .unbounded_send(ReplyMessage::Dispatch(id, Err(acp_error)))
                                .map_err(crate::Error::into_internal_error)?;
                        }
                    }
                }
            },
            Err(error) => {
                // Parse error from transport - send error notification back to remote
                tracing::warn!(?error, "Transport parse error, sending error notification");
                json_rpc_cx.send_error_notification(error)?;
            }
        }
    }
    Ok(())
}

/// Transport incoming actor: Parses bytes into jsonrpcmsg::Message.
///
/// This actor handles transport mechanics:
/// - Reads newline-delimited JSON from the stream
/// - Parses to jsonrpcmsg::Message
/// - Handles parse errors
///
/// This is the transport layer - it has no knowledge of protocol semantics.
pub(super) async fn transport_incoming_actor(
    incoming_bytes: impl AsyncRead,
    transport_tx: mpsc::UnboundedSender<Result<jsonrpcmsg::Message, crate::Error>>,
) -> Result<(), crate::Error> {
    let incoming_bytes = pin!(incoming_bytes);
    let buffered_incoming_bytes = BufReader::new(incoming_bytes);
    let mut incoming_lines = buffered_incoming_bytes.lines();

    while let Some(line) = incoming_lines.next().await {
        let line = line.map_err(crate::Error::into_internal_error)?;
        tracing::trace!(message = %line, "Received JSON-RPC message");

        let message: Result<jsonrpcmsg::Message, _> = serde_json::from_str(&line);
        match message {
            Ok(msg) => {
                transport_tx
                    .unbounded_send(Ok(msg))
                    .map_err(crate::Error::into_internal_error)?;
            }
            Err(_) => {
                transport_tx
                    .unbounded_send(Err(crate::Error::parse_error()))
                    .map_err(crate::Error::into_internal_error)?;
            }
        }
    }
    Ok(())
}

/// Dispatches a JSON-RPC request to the handler.
/// Report an error back to the server if it does not get handled.
async fn dispatch_request(
    json_rpc_cx: &JrConnectionCx,
    request: jsonrpcmsg::Request,
    handler: &mut impl JrMessageHandler,
) -> Result<(), crate::Error> {
    let message = UntypedMessage::new(&request.method, &request.params).expect("well-formed JSON");

    let message_cx = match &request.id {
        Some(id) => MessageAndCx::Request(
            message,
            JrRequestCx::new(json_rpc_cx, request.method.clone(), id.clone()),
        ),
        None => MessageAndCx::Notification(message, json_rpc_cx.clone()),
    };

    match handler.handle_message(message_cx).await? {
        Handled::Yes => {
            tracing::debug!(method = request.method, ?request.id, handler = ?handler.describe_chain(), "Message handled");
            return Ok(());
        }

        Handled::No(m) => {
            tracing::debug!(method = ?request.method, ?request.id, "No suitable handler found");
            m.respond_with_error(crate::Error::method_not_found())
        }
    }
}
