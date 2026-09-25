// Types re-exported from crate root
use futures::StreamExt as _;
use futures::future;
use std::task::Poll;

use crate::jsonrpc::protocol_compat::ProtocolCompat;
use crate::jsonrpc::{
    FramePermit, OutgoingMessage, PendingReplies, RawJsonRpcMessage, TransportFrame, UntypedMessage,
};
use crate::schema::v1::RequestId;

pub type OutgoingMessageTx = super::admission::Sender<OutgoingMessage>;

pub(crate) fn send_raw_message(
    tx: &OutgoingMessageTx,
    message: OutgoingMessage,
) -> Result<(), crate::Error> {
    tracing::debug!(?message, ?tx, "send_raw_message");
    tx.unbounded_send(message)
        .map_err(crate::util::internal_error)
}

async fn publish(
    tx: &super::FrameSender,
    frame: TransportFrame,
    permit: Option<FramePermit>,
) -> Result<(), crate::Error> {
    match permit {
        Some(permit) => tx.send_admitted(frame, permit).await,
        None => tx.send_frame(frame).await,
    }
    .map_err(crate::Error::into_internal_error)
}

async fn publish_notification(
    tx: &super::FrameSender,
    protocol_compat: &ProtocolCompat,
    pending_replies: &PendingReplies,
    untyped: UntypedMessage,
    permit: Option<FramePermit>,
) -> Result<(), crate::Error> {
    if let Some(id) = super::outgoing_cancellation_id(&untyped)
        && pending_replies.cancel_unpublished(&id)
    {
        return Ok(());
    }
    let messages = protocol_compat.outgoing_notification(untyped)?;
    // ProtocolCompat currently emits exactly one notification. A future
    // expansion needs separately admitted charges for each additional output.
    if messages.len() > 1 {
        return Err(crate::util::internal_error(
            "notification expansion exceeds application admission",
        ));
    }
    if let Some(untyped) = messages.into_iter().next() {
        let message = untyped.into_raw_jsonrpc_message(None)?;
        publish(tx, TransportFrame::Single(message), permit).await?;
    }
    Ok(())
}

/// Outgoing protocol actor: Converts application-level OutgoingMessage to protocol-level RawJsonRpcMessage.
///
/// This actor handles JSON-RPC protocol semantics:
/// - Verifies that outgoing requests still have pending response registrations
/// - Converts OutgoingMessage variants to RawJsonRpcMessage
///
/// This is the protocol layer - it has no knowledge of how messages are transported.
pub(super) async fn outgoing_protocol_actor(
    mut outgoing_rx: impl Unpin + super::admission::ReceiverClose<Item = OutgoingMessage>,
    pending_replies: PendingReplies,
    transport_tx: super::FrameSender,
    protocol_compat: ProtocolCompat,
    shutdown: super::IncomingClosed,
) -> Result<(), crate::Error> {
    let mut drain_waiters = Vec::new();

    while let Some(message) = outgoing_rx.next().await {
        tracing::debug!(?message, "outgoing_protocol_actor");
        let (message, permit) = match message {
            OutgoingMessage::Admitted { message, permit } => (*message, Some(permit)),
            message => (message, None),
        };

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
                if let Some((frame, permit)) = completion.complete_admitted(permit) {
                    publish(&transport_tx, frame, permit).await?;
                }
                continue;
            }
            OutgoingMessage::BatchHandlerAttemptComplete { destination } => {
                if let Some((frame, permit)) = destination.finish_handler_attempt_admitted(permit) {
                    publish(&transport_tx, frame, permit).await?;
                }
                continue;
            }
            OutgoingMessage::AbandonedBatchResponse {
                id,
                method,
                destination,
            } => {
                tracing::warn!(
                    ?id,
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
                if let Some((frame, permit)) = destination.abandon_admitted(fallback, permit) {
                    publish(&transport_tx, frame, permit).await?;
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

                if let Some(readiness) = readiness {
                    enum Gate {
                        Ready(Result<(), crate::Error>),
                        Shutdown,
                        Urgent(OutgoingMessage),
                    }
                    let mut readiness = Box::pin(readiness);
                    let mut closing = Box::pin(shutdown.shutdown_requested());
                    let mut skip_request = false;
                    loop {
                        let gate = future::poll_fn(|cx| {
                            if let Poll::Ready(result) = readiness.as_mut().poll(cx) {
                                return Poll::Ready(Gate::Ready(result));
                            }
                            if closing.as_mut().poll(cx).is_ready() {
                                return Poll::Ready(Gate::Shutdown);
                            }
                            match outgoing_rx.poll_urgent(cx) {
                                Poll::Ready(Some(message)) => Poll::Ready(Gate::Urgent(message)),
                                _ => Poll::Pending,
                            }
                        })
                        .await;
                        match gate {
                            Gate::Ready(Ok(())) => break,
                            Gate::Ready(Err(error)) => {
                                tracing::warn!(?id, %method, ?error, "Outgoing request readiness failed");
                                if let Some(pending_reply) = pending_replies.remove(&id) {
                                    pending_reply.fail(error);
                                }
                                skip_request = true;
                                break;
                            }
                            Gate::Shutdown => {
                                if let Some(pending_reply) = pending_replies.remove(&id) {
                                    pending_reply.fail(crate::util::internal_error("connection shut down while waiting for outgoing request readiness"));
                                }
                                skip_request = true;
                                break;
                            }
                            Gate::Urgent(OutgoingMessage::Admitted { message, permit }) => {
                                if let OutgoingMessage::Notification { untyped } = *message {
                                    publish_notification(
                                        &transport_tx,
                                        &protocol_compat,
                                        &pending_replies,
                                        untyped,
                                        Some(permit),
                                    )
                                    .await?;
                                }
                                if !pending_replies.contains(&id) {
                                    skip_request = true;
                                    break;
                                }
                            }
                            Gate::Urgent(_) => unreachable!(
                                "urgent admission only accepts cancellation notifications"
                            ),
                        }
                    }
                    if skip_request {
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
                        tracing::warn!(?id, %method, ?error, "Failed to prepare outgoing request");
                        if let Some(pending_reply) = pending_replies.remove(&id) {
                            pending_reply.fail(error);
                        }
                        continue;
                    }
                };

                if !pending_replies.mark_published(&id) {
                    continue;
                }

                if let Err(error) =
                    publish(&transport_tx, TransportFrame::Single(request), permit).await
                {
                    let error = crate::Error::into_internal_error(error);
                    if let Some(pending_reply) = pending_replies.remove(&id) {
                        pending_reply.fail(error.clone());
                    }
                    return Err(error);
                }
                continue;
            }
            OutgoingMessage::Notification { untyped } => {
                publish_notification(
                    &transport_tx,
                    &protocol_compat,
                    &pending_replies,
                    untyped,
                    permit,
                )
                .await?;
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
                    tracing::warn!(?id, %method, ?error, "Sending error response");
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
            OutgoingMessage::Admitted { .. } => {
                unreachable!("application admission is unwrapped above")
            }
        };

        if let Some((frame, permit)) = destination.complete_admitted(json_rpc_message, permit) {
            publish(&transport_tx, frame, permit).await?;
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
