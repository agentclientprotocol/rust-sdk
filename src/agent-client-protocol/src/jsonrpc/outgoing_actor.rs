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

#[derive(Clone, Debug)]
pub struct OutgoingMessageTx {
    state: Arc<OutgoingMessageState>,
}
#[derive(Debug)]
struct OutgoingMessageState {
    tx: mpsc::UnboundedSender<OutgoingMessage>,
    budget: Option<crate::bounded::Budget>,
}
impl From<mpsc::UnboundedSender<OutgoingMessage>> for OutgoingMessageTx {
    fn from(tx: mpsc::UnboundedSender<OutgoingMessage>) -> Self {
        Self {
            state: Arc::new(OutgoingMessageState { tx, budget: None }),
        }
    }
}
impl OutgoingMessageTx {
    pub(super) fn budget(&self) -> Option<&crate::bounded::Budget> {
        self.state.budget.as_ref()
    }
    pub(super) fn set_budget(&mut self, budget: Option<crate::bounded::Budget>) {
        Arc::get_mut(&mut self.state)
            .expect("configure sender before cloning")
            .budget = budget;
    }
    pub(super) fn reserve(&self) -> Result<Option<crate::bounded::Charge>, crate::Error> {
        self.state
            .budget
            .as_ref()
            .map(crate::bounded::Budget::reserve)
            .transpose()
    }
    pub(super) fn unbounded_send(&self, message: OutgoingMessage) -> Result<(), crate::Error> {
        self.send_with_charge(message, self.reserve()?)
    }
    pub(super) fn send_with_charge(
        &self,
        mut message: OutgoingMessage,
        charge: Option<crate::bounded::Charge>,
    ) -> Result<(), crate::Error> {
        if let Some(budget) = &self.state.budget {
            budget.check_admission()?;
            let charge = charge.expect("bounded producers reserve before conversion");
            message
                .normalize(budget)
                .map_err(|_| budget.fail("outgoing protocol message exceeds bounded limit"))?;
            self.state
                .tx
                .unbounded_send(OutgoingMessage::Admitted {
                    message: Box::new(message),
                    charge,
                })
                .map_err(|_| budget.fail("outgoing protocol actor closed"))
        } else {
            self.state
                .tx
                .unbounded_send(message)
                .map_err(crate::util::internal_error)
        }
    }
}

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
    transport_tx: &crate::bounded::TransportSender,
    completed: CompletedResponseFrame,
) -> Result<(), crate::Error> {
    match transport_tx.send(completed.frame, completed.charges) {
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
    transport_tx: impl Into<crate::bounded::TransportSender>,
    protocol_compat: ProtocolCompat,
) -> Result<(), crate::Error> {
    let transport_tx = transport_tx.into();
    let bounded = matches!(&transport_tx, crate::bounded::TransportSender::Bounded(_));
    let mut drain_waiters = Vec::new();
    let mut receipt_registry = ResponseReceiptRegistry::default();

    while let Some(message) = outgoing_rx.next().await {
        let (message, mut charges) = match message {
            OutgoingMessage::Admitted { message, charge } => (*message, vec![charge]),
            message => (message, vec![]),
        };
        match &message {
            OutgoingMessage::Response { destination, .. }
            | OutgoingMessage::AbandonedBatchResponse { destination, .. }
            | OutgoingMessage::UncorrelatedErrorResponse { destination, .. } => {
                if let super::ResponseDestination::Batch(slot) = destination {
                    slot.state
                        .lock()
                        .expect("batch response accumulator mutex poisoned")
                        .charges
                        .append(&mut charges);
                }
            }
            _ => {}
        }
        tracing::debug!(?message, "outgoing_protocol_actor");

        // Create the message to be sent over the transport
        let (json_rpc_message, destination, receipt) = match message {
            OutgoingMessage::Admitted { .. } => unreachable!("nested admission"),
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

                if let Err(error) = transport_tx.send(
                    TransportFrame::Single(request),
                    std::mem::take(&mut charges),
                ) {
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
                        if bounded {
                            return Err(error);
                        }
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
                            if bounded {
                                return Err(error);
                            }
                            tracing::warn!(
                                ?error,
                                "Dropping outgoing notification after serialization failed"
                            );
                            continue;
                        }
                    };
                    transport_tx
                        .send(
                            TransportFrame::Single(message),
                            std::mem::take(&mut charges),
                        )
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
        if let Some(mut frame) = destination.complete(json_rpc_message, receipt) {
            frame.charges.append(&mut charges);
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
            charges: vec![],
        };

        assert!(enqueue_completed_response(&transport_tx.into(), completed).is_err());
        let (first_result, second_result) = block_on(join(first_receipt, second_receipt));
        assert!(first_result.is_err());
        assert!(second_result.is_err());
    }
}
