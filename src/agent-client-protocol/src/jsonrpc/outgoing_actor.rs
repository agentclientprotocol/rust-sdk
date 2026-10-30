// Types re-exported from crate root
use futures::StreamExt as _;
use futures::channel::mpsc;

use crate::RawJsonRpcId;
use crate::jsonrpc::protocol_compat::ProtocolCompat;
use crate::jsonrpc::{OutgoingMessage, PendingReplies, RawJsonRpcMessage, TransportFrame};

pub type OutgoingMessageTx = mpsc::UnboundedSender<OutgoingMessage>;

pub(crate) fn send_raw_message(
    tx: &OutgoingMessageTx,
    message: OutgoingMessage,
) -> Result<(), crate::Error> {
    let (kind, method, id) = message.log_summary();
    tracing::debug!(kind, method, ?id, ?tx, "send_raw_message");
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
    foreground_done: super::SharedCompletionSignal,
) -> Result<(), crate::Error> {
    let mut drain_waiters = Vec::new();

    while let Some(message) = outgoing_rx.next().await {
        let (kind, method, id) = message.log_summary();
        tracing::debug!(kind, method, ?id, "outgoing_protocol_actor");

        // Create the message to be sent over the transport
        let (json_rpc_message, destination) = match message {
            OutgoingMessage::CloseAfterDraining { done } => {
                // Reject later sends while preserving every message that was
                // already accepted into this receiver's buffer.
                outgoing_rx.close();
                drain_waiters.push(done);
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

                if let Some(readiness) = readiness {
                    // A route-installation gate may depend on an application
                    // handler that never returns. Foreground success cancels
                    // unresolved gates, but ready gates still publish in FIFO
                    // order; shutdown must never bypass route readiness.
                    // Poll a fresh clone for each gate: a Shared handle that
                    // returned Ready cannot itself be polled a second time.
                    let result =
                        match futures::future::select(Box::pin(readiness), foreground_done.clone())
                            .await
                        {
                            futures::future::Either::Left((result, _)) => result,
                            futures::future::Either::Right(((), _)) => {
                                Err(crate::Error::internal_error()
                                    .data("foreground completed before outgoing request readiness"))
                            }
                        };
                    if let Err(error) = result {
                        tracing::warn!(
                            ?id,
                            %method,
                            error = ?crate::util::redacted_error(error.code),
                            "Outgoing request readiness failed"
                        );
                        if let Some(pending_reply) = pending_replies.remove(&id) {
                            pending_reply.fail(error);
                        }
                        continue;
                    }
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
                        tracing::warn!(?id, %method, error = ?crate::util::redacted_error(error.code), "Failed to prepare outgoing request");
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
                            error = ?crate::util::redacted_error(error.code),
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
                                error = ?crate::util::redacted_error(error.code),
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
                    tracing::debug!(?id, "Sending success response");
                    (RawJsonRpcMessage::response(id, Ok(value)), destination)
                }
                Err(error) => {
                    tracing::warn!(?id, %method, error = ?crate::util::redacted_error(error.code), "Sending error response");
                    (RawJsonRpcMessage::response(id, Err(error)), destination)
                }
            },
            OutgoingMessage::UncorrelatedErrorResponse { error, destination } => {
                // JSON-RPC reports parse/invalid-request errors with id null when
                // they cannot be correlated to a specific request.
                (
                    RawJsonRpcMessage::response(RawJsonRpcId::Null, Err(error)),
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
