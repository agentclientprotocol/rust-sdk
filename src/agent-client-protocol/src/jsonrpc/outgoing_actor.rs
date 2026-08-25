// Types re-exported from crate root
use std::sync::{Arc, Weak};

use futures::StreamExt as _;
use futures::channel::mpsc;

use crate::jsonrpc::protocol_compat::ProtocolCompat;
use crate::jsonrpc::{
    CompletedResponseFrame, OutgoingMessage, PendingReplies, RawJsonRpcMessage,
    ResponseReceiptSender, ResponseReceiptState, TransportFrame, response_receipt_teardown_error,
};
use crate::schema::v1::RequestId;

pub type OutgoingMessageTx = mpsc::UnboundedSender<OutgoingMessage>;

pub(crate) fn send_raw_message(
    tx: &OutgoingMessageTx,
    message: OutgoingMessage,
) -> Result<(), crate::Error> {
    tracing::debug!(?message, ?tx, "send_raw_message");
    tx.unbounded_send(message)
        .map_err(crate::util::internal_error)
}

#[derive(Default)]
struct ResponseReceiptRegistry {
    pending: Vec<Weak<ResponseReceiptState>>,
}

impl ResponseReceiptRegistry {
    fn register(&mut self, sender: &ResponseReceiptSender) {
        self.pending.retain(|state| state.strong_count() != 0);
        self.pending.push(Arc::downgrade(&sender.state));
    }
}

impl Drop for ResponseReceiptRegistry {
    fn drop(&mut self) {
        for state in self.pending.drain(..).filter_map(|state| state.upgrade()) {
            state.resolve(Err(response_receipt_teardown_error()));
        }
    }
}

fn enqueue_completed_response(
    transport_tx: &mpsc::UnboundedSender<TransportFrame>,
    completed: CompletedResponseFrame,
) -> Result<(), crate::Error> {
    match transport_tx.unbounded_send(completed.frame) {
        Ok(()) => {
            for receipt in completed.receipts {
                receipt.resolve(Ok(()));
            }
            Ok(())
        }
        Err(error) => {
            let error = crate::Error::into_internal_error(error);
            for receipt in completed.receipts {
                receipt.resolve(Err(error.clone()));
            }
            Err(error)
        }
    }
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
    let mut receipt_registry = ResponseReceiptRegistry::default();

    while let Some(message) = outgoing_rx.next().await {
        tracing::debug!(?message, "outgoing_protocol_actor");

        // Create the message to be sent over the transport
        let (json_rpc_message, destination, receipt) = match message {
            OutgoingMessage::CloseAfterDraining { done } => {
                // Reject later sends while preserving every message that was
                // already accepted into this receiver's buffer.
                outgoing_rx.close();
                drain_waiters.push(done);
                continue;
            }
            OutgoingMessage::BatchDispatchComplete { completion } => {
                if let Some(frame) = completion.complete() {
                    enqueue_completed_response(&transport_tx, frame)?;
                }
                continue;
            }
            OutgoingMessage::BatchHandlerAttemptComplete { destination } => {
                if let Some(frame) = destination.finish_handler_attempt() {
                    enqueue_completed_response(&transport_tx, frame)?;
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
                if let Some(frame) = destination.abandon(fallback) {
                    enqueue_completed_response(&transport_tx, frame)?;
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
                        ?id,
                        %method,
                        ?error,
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
                        tracing::warn!(?id, %method, ?error, "Failed to prepare outgoing request");
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
                            ?error,
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
                                ?error,
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
                receipt,
            } => match protocol_compat.outgoing_response_to(&id, &method, response) {
                Ok(value) => {
                    tracing::debug!(?id, "Sending success response");
                    (
                        RawJsonRpcMessage::response(id, Ok(value)),
                        destination,
                        receipt,
                    )
                }
                Err(error) => {
                    tracing::warn!(?id, %method, ?error, "Sending error response");
                    (
                        RawJsonRpcMessage::response(id, Err(error)),
                        destination,
                        receipt,
                    )
                }
            },
            OutgoingMessage::UncorrelatedErrorResponse { error, destination } => {
                // JSON-RPC reports parse/invalid-request errors with id null when
                // they cannot be correlated to a specific request.
                (
                    RawJsonRpcMessage::response(RequestId::Null, Err(error)),
                    destination,
                    None,
                )
            }
        };

        if let Some(receipt) = receipt.as_ref() {
            receipt_registry.register(receipt);
        }
        if let Some(frame) = destination.complete(json_rpc_message, receipt) {
            enqueue_completed_response(&transport_tx, frame)?;
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

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use futures::future::join;

    use super::*;

    #[test]
    fn actor_teardown_fails_every_registered_response_receipt() {
        let (first_sender, first_receipt) = ResponseReceiptSender::channel();
        let (second_sender, second_receipt) = ResponseReceiptSender::channel();
        let mut registry = ResponseReceiptRegistry::default();
        registry.register(&first_sender);
        registry.register(&second_sender);

        drop(registry);

        let (first_result, second_result) = block_on(join(first_receipt, second_receipt));
        assert!(first_result.is_err());
        assert!(second_result.is_err());
        drop((first_sender, second_sender));
    }

    #[test]
    fn response_transport_queue_failure_fails_every_receipt() {
        let (transport_tx, transport_rx) = mpsc::unbounded();
        drop(transport_rx);
        let (first_sender, first_receipt) = ResponseReceiptSender::channel();
        let (second_sender, second_receipt) = ResponseReceiptSender::channel();
        let completed = CompletedResponseFrame {
            frame: TransportFrame::Single(RawJsonRpcMessage::response(
                RequestId::Null,
                Ok(serde_json::Value::Null),
            )),
            receipts: vec![first_sender, second_sender],
        };

        assert!(enqueue_completed_response(&transport_tx, completed).is_err());
        let (first_result, second_result) = block_on(join(first_receipt, second_receipt));
        assert!(first_result.is_err());
        assert!(second_result.is_err());
    }
}
