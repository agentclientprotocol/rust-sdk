// Types re-exported from crate root
use futures::StreamExt as _;
use futures::channel::mpsc;

use crate::jsonrpc::protocol_compat::ProtocolCompat;
use crate::jsonrpc::{OutgoingMessage, PendingReplies, RawJsonRpcMessage, TransportFrame};
use crate::schema::v1::RequestId;

pub type OutgoingMessageTx = mpsc::UnboundedSender<OutgoingMessage>;

fn outgoing_message_kind(message: &OutgoingMessage) -> &'static str {
    match message {
        OutgoingMessage::CloseAfterDraining { .. } => "close_after_draining",
        OutgoingMessage::BatchDispatchComplete { .. } => "batch_dispatch_complete",
        OutgoingMessage::BatchHandlerAttemptComplete { .. } => "batch_handler_attempt_complete",
        OutgoingMessage::AbandonedBatchResponse { .. } => "abandoned_batch_response",
        OutgoingMessage::Request { .. } => "request",
        OutgoingMessage::Notification { .. } => "notification",
        OutgoingMessage::Response { .. } => "response",
        OutgoingMessage::UncorrelatedErrorResponse { .. } => "uncorrelated_error_response",
    }
}

pub(crate) fn send_raw_message(
    tx: &OutgoingMessageTx,
    message: OutgoingMessage,
) -> Result<(), crate::Error> {
    tracing::debug!(
        message_kind = outgoing_message_kind(&message),
        "send_raw_message"
    );
    tx.unbounded_send(message)
        .map_err(crate::util::internal_error)
}

/// Outgoing protocol actor: Converts application-level OutgoingMessage to protocol-level RawJsonRpcMessage.
///
/// This actor handles JSON-RPC protocol semantics:
/// - Verifies that outgoing requests still have pending response registrations
/// - Converts OutgoingMessage variants to RawJsonRpcMessage
///
/// This is the protocol layer - it has no knowledge of how messages are transported.
pub(super) async fn outgoing_protocol_actor(
    mut outgoing_rx: mpsc::UnboundedReceiver<OutgoingMessage>,
    pending_replies: PendingReplies,
    transport_tx: mpsc::UnboundedSender<TransportFrame>,
    protocol_compat: ProtocolCompat,
) -> Result<(), crate::Error> {
    let mut drain_waiters = Vec::new();

    while let Some(message) = outgoing_rx.next().await {
        tracing::debug!(
            message_kind = outgoing_message_kind(&message),
            "outgoing_protocol_actor"
        );

        // Create the message to be sent over the transport
        let (json_rpc_message, destination) = match message {
            OutgoingMessage::CloseAfterDraining { done } => {
                // Reject later sends while preserving every message that was
                // already accepted into this receiver's buffer.
                outgoing_rx.close();
                drain_waiters.push(done);
                continue;
            }
            OutgoingMessage::BatchDispatchComplete { completion } => {
                if let Some(frame) = completion.complete() {
                    transport_tx
                        .unbounded_send(frame)
                        .map_err(crate::Error::into_internal_error)?;
                }
                continue;
            }
            OutgoingMessage::BatchHandlerAttemptComplete { destination } => {
                if let Some(frame) = destination.finish_handler_attempt() {
                    transport_tx
                        .unbounded_send(frame)
                        .map_err(crate::Error::into_internal_error)?;
                }
                continue;
            }
            OutgoingMessage::AbandonedBatchResponse {
                id,
                method,
                destination,
            } => {
                tracing::warn!(
                    %method,
                    "Completing abandoned JSON-RPC batch request with Internal Error"
                );
                let fallback = protocol_compat.outgoing_response_to(
                    &id,
                    &method,
                    Err(crate::Error::internal_error().data(format!(
                        "request handler dropped its responder for `{method}`"
                    ))),
                );
                let fallback = RawJsonRpcMessage::response(id, fallback);
                if let Some(frame) = destination.abandon(fallback) {
                    transport_tx
                        .unbounded_send(frame)
                        .map_err(crate::Error::into_internal_error)?;
                }
                continue;
            }
            OutgoingMessage::Request {
                id,
                method,
                untyped,
                remote_style,
                readiness,
            } => {
                // Requests register their response destination synchronously
                // before entering this queue. EOF removes that registration,
                // so skip work that can no longer receive a response.
                if !pending_replies.contains(&id) {
                    continue;
                }

                if let Some(readiness) = readiness
                    && let Err(error) = readiness.await
                {
                    tracing::warn!(
                        %method,
                        error_code = ?&error.code,
                        "Outgoing request readiness failed"
                    );
                    if let Some(pending_reply) = pending_replies.remove(&id) {
                        pending_reply.fail(error);
                    }
                    continue;
                }

                if !pending_replies.contains(&id) {
                    continue;
                }

                let request = match protocol_compat
                    .outgoing_message(untyped, remote_style)
                    .and_then(|untyped| remote_style.transform_outgoing_message(untyped))
                    .and_then(|untyped| untyped.into_raw_jsonrpc_message(Some(id.clone())))
                {
                    Ok(request) => request,
                    Err(error) => {
                        tracing::warn!(%method, error_code = ?&error.code, "Failed to prepare outgoing request");
                        if let Some(pending_reply) = pending_replies.remove(&id) {
                            pending_reply.fail(error);
                        }
                        continue;
                    }
                };

                if !pending_replies.contains(&id) {
                    continue;
                }

                if let Err(error) = transport_tx.unbounded_send(TransportFrame::Single(request)) {
                    let error = crate::Error::into_internal_error(error);
                    if let Some(pending_reply) = pending_replies.remove(&id) {
                        pending_reply.fail(error.clone());
                    }
                    return Err(error);
                }
                continue;
            }
            OutgoingMessage::Notification { untyped } => {
                let messages = match protocol_compat.outgoing_notification(untyped) {
                    Ok(messages) => messages,
                    Err(error) => {
                        tracing::warn!(
                            error_code = ?&error.code,
                            "Dropping outgoing notification after preparation failed"
                        );
                        continue;
                    }
                };

                for untyped in messages {
                    let message = match untyped.into_raw_jsonrpc_message(None) {
                        Ok(message) => message,
                        Err(error) => {
                            tracing::warn!(
                                error_code = ?&error.code,
                                "Dropping outgoing notification after serialization failed"
                            );
                            continue;
                        }
                    };
                    transport_tx
                        .unbounded_send(TransportFrame::Single(message))
                        .map_err(crate::Error::into_internal_error)?;
                }
                continue;
            }
            OutgoingMessage::Response {
                id,
                method,
                response,
                destination,
            } => match protocol_compat.outgoing_response_to(&id, &method, response) {
                Ok(value) => {
                    tracing::debug!(%method, "Sending success response");
                    (RawJsonRpcMessage::response(id, Ok(value)), destination)
                }
                Err(error) => {
                    tracing::warn!(%method, error_code = ?&error.code, "Sending error response");
                    (RawJsonRpcMessage::response(id, Err(error)), destination)
                }
            },
            OutgoingMessage::UncorrelatedErrorResponse { error, destination } => {
                // JSON-RPC reports parse/invalid-request errors with id null when
                // they cannot be correlated to a specific request.
                (
                    RawJsonRpcMessage::response(RequestId::Null, Err(error)),
                    destination,
                )
            }
        };

        if let Some(frame) = destination.complete(json_rpc_message) {
            transport_tx
                .unbounded_send(frame)
                .map_err(crate::Error::into_internal_error)?;
        }
    }

    // Closing the raw queue lets the transport actor finish all buffered
    // writes. The caller separately awaits that transport future before
    // treating the drain as complete.
    drop(transport_tx);
    for done in drain_waiters {
        let _ = done.send(());
    }
    Ok(())
}
