use super::*;
use crate::role::UntypedRole;
use serde_json::json;

#[test]
fn preparation_configuration_and_drop_send_nothing() {
    let mut fixture = Fixture::new();
    let cancellation = RequestCancellation::new();
    cancellation.cancel();
    let request = fixture
        .connection
        .prepare_request_to(UntypedRole, request())
        .forward_cancellation_from(cancellation)
        .map(|response| Ok(response.to_string()));
    assert_eq!(request.method(), "prepared");
    assert!(!fixture.pending_replies.contains(request.id()));
    let handle = request.cancellation_handle();
    handle.cancel().unwrap();
    drop(handle.clone());
    assert!(fixture.message_rx.next().now_or_never().is_none());
    assert!(fixture.task_rx.next().now_or_never().is_none());
    drop(request);
    handle.cancel().unwrap();
    assert!(fixture.message_rx.next().now_or_never().is_none());
}

#[test]
fn blocking_publishes_before_polling_and_drop_cancels() {
    let mut fixture = Fixture::new();
    let prepared = fixture.connection.prepare_request(request());
    let id = prepared.id().clone();
    let response = prepared.block_task();
    fixture
        .connection
        .send_notification(UntypedMessage::new("following", json!({})).unwrap())
        .unwrap();
    assert!(matches!(
        fixture.next_message(),
        OutgoingMessage::Request { id: sent_id, .. } if sent_id == id
    ));
    assert!(matches!(
        fixture.next_message(),
        OutgoingMessage::Notification { untyped } if untyped.method() == "following"
    ));
    assert!(
        !fixture.pending_replies.inner.lock().unwrap().replies[&id]
            .ordering
            .is_ordered()
    );
    drop(response);
    let OutgoingMessage::Notification { untyped } = fixture.next_message() else {
        panic!("dropping the unpolled response future must cancel");
    };
    assert_eq!(untyped.method(), "$/cancel_request");
    assert_eq!(
        untyped.params()["requestId"],
        serde_json::to_value(id).unwrap()
    );
}

#[test]
fn blocking_maps_the_response_without_holding_dispatch() {
    let mut fixture = Fixture::new();
    let prepared = fixture
        .connection
        .prepare_request(request())
        .map(|value| Ok(value["value"].as_u64().unwrap()));
    let id = prepared.id().clone();
    let response = prepared.block_task();
    fixture.next_message();
    assert!(fixture.route(id, Ok(json!({"value": 42}))).is_none());
    assert_eq!(futures::executor::block_on(response).unwrap(), 42);
    assert!(fixture.message_rx.next().now_or_never().is_none());
}

#[test]
fn detach_publishes_without_ordering_or_automatic_cancellation() {
    let mut fixture = Fixture::new();
    let prepared = fixture.connection.prepare_request(request());
    let id = prepared.id().clone();
    let handle = prepared.cancellation_handle();
    prepared.detach().unwrap();
    fixture.next_message();
    assert!(fixture.message_rx.next().now_or_never().is_none());
    assert!(
        fixture
            .route(id, Err(crate::Error::invalid_params()))
            .is_none()
    );
    handle.cancel().unwrap();
    assert!(fixture.message_rx.next().now_or_never().is_none());
    assert!(fixture.task_rx.next().now_or_never().is_none());
}

#[test]
fn detached_handles_preserve_once_only_control_until_response_routing() {
    for eager in [false, true] {
        for result in [
            Ok(json!({"value": 42})),
            Err(crate::Error::request_cancelled()),
        ] {
            let mut fixture = Fixture::new();
            let (id, handle) = if eager {
                let sent = fixture.connection.send_request(request());
                let id = sent.id().clone();
                let handle = sent.cancellation_handle();
                sent.detach();
                (id, handle)
            } else {
                let prepared = fixture.connection.prepare_request(request());
                let id = prepared.id().clone();
                let handle = prepared.cancellation_handle();
                prepared.detach().unwrap();
                (id, handle)
            };
            assert!(matches!(
                fixture.next_message(),
                OutgoingMessage::Request { id: sent_id, .. } if sent_id == id
            ));
            assert!(fixture.message_rx.next().now_or_never().is_none());
            handle.cancel().unwrap();
            handle.clone().cancel().unwrap();
            let OutgoingMessage::Notification { untyped } = fixture.next_message() else {
                panic!("a detached pending request must retain explicit cancellation");
            };
            assert_eq!(untyped.method(), "$/cancel_request");
            assert_eq!(
                untyped.params()["requestId"],
                serde_json::to_value(&id).unwrap()
            );
            assert!(fixture.message_rx.next().now_or_never().is_none());
            assert!(fixture.route(id, result).is_none());
            handle.cancel().unwrap();
            drop(handle);
            assert!(fixture.message_rx.next().now_or_never().is_none());
            assert!(fixture.task_rx.next().now_or_never().is_none());
        }
    }
}

#[test]
fn detached_handles_disarm_on_eof_without_a_response_consumer() {
    let mut fixture = Fixture::new();
    let prepared = fixture.connection.prepare_request(request());
    let handle = prepared.cancellation_handle();
    prepared.detach().unwrap();
    fixture.next_message();
    fixture.connection.incoming_closed.begin_close();
    assert_eq!(fixture.pending_replies.close_incoming(), 1);
    handle.cancel().unwrap();
    handle.clone().cancel().unwrap();
    drop(handle);
    assert!(fixture.message_rx.next().now_or_never().is_none());
    assert!(fixture.task_rx.next().now_or_never().is_none());
}

#[test]
fn callback_ordering_is_installed_before_publication_and_task_polling() {
    for result in [
        Ok(json!({"value": 42})),
        Err(crate::Error::invalid_params()),
    ] {
        let mut fixture = Fixture::new();
        let prepared = fixture.connection.prepare_request(request());
        let id = prepared.id().clone();
        let handle = prepared.cancellation_handle();
        let expected = result.clone();
        prepared
            .on_receiving_result(async move |actual| {
                assert_eq!(actual, expected);
                Ok(())
            })
            .unwrap();
        fixture.next_message();
        let mut acknowledgment = fixture.route(id, result).unwrap();
        assert_eq!(acknowledgment.try_recv().unwrap(), None);
        handle.cancel().unwrap();
        assert!(fixture.message_rx.next().now_or_never().is_none());
        let task = fixture.next_task();
        futures::executor::block_on(task.run_for_test()).unwrap();
        futures::executor::block_on(acknowledgment).unwrap();
    }
}

#[test]
fn callback_failure_releases_the_barrier_and_fails_the_task() {
    let mut fixture = Fixture::new();
    let prepared = fixture.connection.prepare_request(request());
    let id = prepared.id().clone();
    prepared
        .on_receiving_result(async |_| Err(crate::Error::invalid_params()))
        .unwrap();
    fixture.next_message();
    let acknowledgment = fixture.route(id, Ok(json!({}))).unwrap();
    let error = futures::executor::block_on(fixture.next_task().run_for_test()).unwrap_err();
    assert_eq!(error.code, crate::ErrorCode::InvalidParams);
    futures::executor::block_on(acknowledgment).unwrap();
}

#[test]
fn task_registration_failure_does_not_publish_or_cancel() {
    let mut fixture = Fixture::new();
    let prepared = fixture.connection.prepare_request(request());
    let id = prepared.id().clone();
    let handle = prepared.cancellation_handle();
    fixture.task_rx.close();
    assert!(prepared.on_receiving_result(async |_| Ok(())).is_err());
    handle.cancel().unwrap();
    assert!(!fixture.pending_replies.contains(&id));
    assert!(fixture.message_rx.next().now_or_never().is_none());
}

#[test]
fn publication_failures_reach_each_consumer_without_cancellation() {
    for failure in [
        Failure::Serialization,
        Failure::OutgoingClosed,
        Failure::IncomingClosed,
        Failure::DriverStopped,
    ] {
        for consumer in 0..3 {
            let mut fixture = Fixture::new();
            let prepared = if matches!(failure, Failure::Serialization) {
                fixture.connection.prepare_request(FailingRequest)
            } else {
                fixture.connection.prepare_request(request())
            };
            let id = prepared.id().clone();
            let handle = prepared.cancellation_handle();
            match failure {
                Failure::Serialization => {}
                Failure::OutgoingClosed => fixture.message_rx.close(),
                Failure::IncomingClosed => {
                    fixture.connection.incoming_closed.begin_close();
                    fixture.pending_replies.close_incoming();
                }
                Failure::DriverStopped => {
                    fixture.pending_replies = PendingReplies::default();
                }
            }
            match consumer {
                0 => {
                    let error = futures::executor::block_on(prepared.block_task()).unwrap_err();
                    assert_eq!(
                        is_incoming_transport_closed(&error),
                        matches!(failure, Failure::IncomingClosed)
                    );
                }
                1 => {
                    prepared
                        .on_receiving_result(async move |result| {
                            let error = result.unwrap_err();
                            assert_eq!(
                                is_incoming_transport_closed(&error),
                                matches!(failure, Failure::IncomingClosed)
                            );
                            Ok(())
                        })
                        .unwrap();
                    futures::executor::block_on(fixture.next_task().run_for_test()).unwrap();
                }
                2 => {
                    let error = prepared.detach().unwrap_err();
                    assert_eq!(
                        is_incoming_transport_closed(&error),
                        matches!(failure, Failure::IncomingClosed)
                    );
                }
                _ => unreachable!(),
            }
            handle.cancel().unwrap();
            assert!(!fixture.pending_replies.contains(&id));
            assert!(!matches!(
                fixture.message_rx.next().now_or_never(),
                Some(Some(_))
            ));
        }
    }
}

#[test]
fn eof_after_publication_runs_the_callback_without_a_response_barrier() {
    let mut fixture = Fixture::new();
    let prepared = fixture.connection.prepare_request(request());
    let handle = prepared.cancellation_handle();
    prepared
        .on_receiving_result(async |result| {
            assert!(is_incoming_transport_closed(&result.unwrap_err()));
            Ok(())
        })
        .unwrap();
    fixture.next_message();
    fixture.connection.incoming_closed.begin_close();
    assert_eq!(fixture.pending_replies.close_incoming(), 1);
    handle.cancel().unwrap();
    futures::executor::block_on(fixture.next_task().run_for_test()).unwrap();
    assert!(fixture.message_rx.next().now_or_never().is_none());
}

#[test]
fn forwarding_and_success_callbacks_preserve_ordering_and_peer_errors() {
    for success_callback in [false, true] {
        for result in [
            Ok(json!({"value": 42})),
            Err(crate::Error::invalid_params()),
        ] {
            let mut fixture = Fixture::new();
            let mut upstream = Fixture::new();
            let registry = RequestCancellationRegistry::new();
            let responder = Responder::new(
                upstream.connection.message_tx.clone(),
                "upstream".into(),
                RequestId::Str("upstream-id".into()),
                &registry,
                ResponseDestination::individual(),
            );
            let prepared = fixture.connection.prepare_request(request());
            let id = prepared.id().clone();
            if success_callback {
                prepared
                    .on_receiving_ok_result(responder, async |value, responder| {
                        responder.respond(value)
                    })
                    .unwrap();
            } else {
                prepared.forward_response_to(responder).unwrap();
            }
            fixture.next_message();
            let acknowledgment = fixture.route(id, result.clone()).unwrap();
            futures::executor::block_on(fixture.next_task().run_for_test()).unwrap();
            futures::executor::block_on(acknowledgment).unwrap();
            let OutgoingMessage::Response { response, .. } = upstream.next_message() else {
                panic!("expected a forwarded response");
            };
            assert_eq!(response, result);
        }
    }
}

#[test]
fn consumer_waits_for_publication_before_forwarding_cancellation() {
    let mut fixture = Fixture::new();
    let cancellation = RequestCancellation::new();
    cancellation.cancel();
    let prepared = fixture
        .connection
        .prepare_request(request())
        .forward_cancellation_from(cancellation);
    let id = prepared.id().clone();
    let handle = prepared.cancellation_handle();
    let published_tx = prepared.register_consumer(async |_| Ok(())).unwrap();
    let mut task = Box::pin(fixture.next_task().run_for_test());
    assert!(task.as_mut().now_or_never().is_none());
    assert!(fixture.message_rx.next().now_or_never().is_none());

    prepared.publication.publish().unwrap();
    published_tx.send(prepared.sent).unwrap();
    assert!(task.as_mut().now_or_never().is_none());
    assert!(matches!(
        fixture.next_message(),
        OutgoingMessage::Request { .. }
    ));
    let OutgoingMessage::Notification { untyped } = fixture.next_message() else {
        panic!("expected cancellation after the request");
    };
    assert_eq!(untyped.method(), "$/cancel_request");
    assert_eq!(
        untyped.params()["requestId"],
        serde_json::to_value(&id).unwrap()
    );
    handle.cancel().unwrap();
    assert!(fixture.message_rx.next().now_or_never().is_none());
    let acknowledgment = fixture
        .route(id, Err(crate::Error::request_cancelled()))
        .unwrap();
    futures::executor::block_on(task).unwrap();
    futures::executor::block_on(acknowledgment).unwrap();
}

#[test]
fn consumer_destruction_before_publication_cannot_overtake_or_lose_cancellation() {
    for arm_before_drop in [false, true] {
        for poll_before_drop in [false, true] {
            let mut fixture = Fixture::new();
            let prepared = fixture.connection.prepare_request(request());
            let id = prepared.id().clone();
            let published_tx = prepared.register_consumer(async |_| Ok(())).unwrap();
            let mut task = Box::pin(fixture.next_task().run_for_test());
            if poll_before_drop {
                assert!(task.as_mut().now_or_never().is_none());
            }
            if arm_before_drop {
                // Reproduce destruction in the arm-to-enqueue interval.
                prepared.publication.pending_reply.cancellation_disarm.arm();
            }
            drop(task);
            assert!(fixture.message_rx.next().now_or_never().is_none());

            prepared.publication.publish().unwrap();
            drop(published_tx.send(prepared.sent));
            assert!(matches!(
                fixture.next_message(),
                OutgoingMessage::Request { id: sent_id, .. } if sent_id == id
            ));
            let OutgoingMessage::Notification { untyped } = fixture.next_message() else {
                panic!("lost response consumer must cancel after publication");
            };
            assert_eq!(untyped.method(), "$/cancel_request");
            assert_eq!(
                untyped.params()["requestId"],
                serde_json::to_value(id).unwrap()
            );
            assert!(fixture.message_rx.next().now_or_never().is_none());
        }
    }
}

#[test]
fn response_before_consumer_handoff_keeps_cancellation_disarmed() {
    let mut fixture = Fixture::new();
    let prepared = fixture.connection.prepare_request(request());
    let id = prepared.id().clone();
    let published_tx = prepared.register_consumer(async |_| Ok(())).unwrap();
    drop(fixture.next_task());
    prepared.publication.publish().unwrap();
    fixture.next_message();
    let acknowledgment = fixture.route(id, Ok(json!({}))).unwrap();
    drop(published_tx.send(prepared.sent));
    assert!(futures::executor::block_on(acknowledgment).is_err());
    assert!(fixture.message_rx.next().now_or_never().is_none());
}

#[test]
fn publication_activation_preserves_cancellation_and_settlement() {
    #[derive(Clone, Copy)]
    enum Outcome {
        Pending,
        Response,
        Eof,
    }

    for outcome in [Outcome::Pending, Outcome::Response, Outcome::Eof] {
        let mut fixture = Fixture::new();
        let prepared = fixture.connection.prepare_request(request());
        let id = prepared.id().clone();
        let handle = prepared.cancellation_handle();
        let PreparedRequest { sent, publication } = prepared;
        let RequestPublication {
            message,
            pending_reply,
            message_tx,
            pending_replies,
            incoming_closed,
        } = publication;
        let cancellation_disarm = pending_reply.cancellation_disarm.clone();
        pending_replies
            .subscribe(id.clone(), pending_reply, &incoming_closed)
            .unwrap();
        handle.cancel().unwrap();
        assert!(fixture.message_rx.next().now_or_never().is_none());
        message_tx.unbounded_send(message.unwrap()).unwrap();
        assert!(matches!(
            fixture.next_message(),
            OutgoingMessage::Request { id: sent_id, .. } if sent_id == id
        ));

        handle.cancel().unwrap();
        assert!(fixture.message_rx.next().now_or_never().is_none());
        // Force each outcome in the enqueue-to-activation interval of publication.
        match outcome {
            Outcome::Pending => {
                assert!(cancellation_disarm.arm());
                handle.cancel().unwrap();
                handle.clone().cancel().unwrap();
                let OutgoingMessage::Notification { untyped } = fixture.next_message() else {
                    panic!("activation must preserve later cancellation");
                };
                assert_eq!(untyped.method(), "$/cancel_request");
                assert_eq!(
                    untyped.params()["requestId"],
                    serde_json::to_value(&id).unwrap()
                );
                assert!(fixture.route(id, Ok(json!({"value": 42}))).is_none());
            }
            Outcome::Response => {
                assert!(fixture.route(id, Ok(json!({"value": 42}))).is_none());
                assert!(!cancellation_disarm.arm());
            }
            Outcome::Eof => {
                incoming_closed.begin_close();
                assert_eq!(fixture.pending_replies.close_incoming(), 1);
                assert!(!cancellation_disarm.arm());
            }
        }
        handle.cancel().unwrap();
        let response = futures::executor::block_on(sent.block_task());
        if matches!(outcome, Outcome::Eof) {
            assert!(is_incoming_transport_closed(&response.unwrap_err()));
        } else {
            assert_eq!(response.unwrap(), json!({"value": 42}));
        }
        assert!(fixture.message_rx.next().now_or_never().is_none());
    }
}

#[test]
fn only_the_first_cancellation_attempt_reports_send_failure() {
    let mut fixture = Fixture::new();
    let prepared = fixture.connection.prepare_request(request());
    let handle = prepared.cancellation_handle();
    let response = prepared.block_task();
    fixture.next_message();
    fixture.message_rx.close();
    assert!(handle.cancel().is_err());
    handle.clone().cancel().unwrap();
    drop(response);
    assert!(!matches!(
        fixture.message_rx.next().now_or_never(),
        Some(Some(_))
    ));
}

#[test]
fn cancellation_handle_traits_do_not_depend_on_the_response_type() {
    fn assert_traits<T: Clone + Debug + Send + Sync>() {}
    assert_traits::<crate::RequestCancellationHandle>();
    let fixture = Fixture::new();
    let prepared = fixture
        .connection
        .prepare_request(request())
        .map(|_| Ok(std::rc::Rc::new(())));
    let handle: crate::RequestCancellationHandle = prepared.cancellation_handle();
    drop(prepared);
    handle.cancel().unwrap();
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn v2_context_exposes_both_preparation_methods() {
    let mut fixture = Fixture::new();
    let connection = V2ConnectionTo {
        inner: fixture.connection.clone(),
    };
    let first = connection.prepare_request(request());
    let second = connection.prepare_request_to(UntypedRole, request());
    assert!(fixture.message_rx.next().now_or_never().is_none());
    first.detach().unwrap();
    second.detach().unwrap();
    assert!(matches!(
        fixture.next_message(),
        OutgoingMessage::Request { .. }
    ));
    assert!(matches!(
        fixture.next_message(),
        OutgoingMessage::Request { .. }
    ));
}

struct Fixture {
    connection: ConnectionTo<UntypedRole>,
    message_rx: mpsc::UnboundedReceiver<OutgoingMessage>,
    task_rx: mpsc::UnboundedReceiver<Task>,
    pending_replies: PendingReplies,
}

impl Fixture {
    fn new() -> Self {
        let (message_tx, message_rx) = mpsc::unbounded();
        let (task_tx, task_rx) = mpsc::unbounded();
        let (dynamic_handler_tx, _) = mpsc::unbounded();
        let pending_replies = PendingReplies::default();
        let connection = ConnectionTo::new(
            UntypedRole,
            message_tx,
            task_tx,
            dynamic_handler_tx,
            future::ready(Ok(())).boxed().shared(),
            pending_replies.registrar(),
            ProtocolMode::disabled(),
        );
        Self {
            connection,
            message_rx,
            task_rx,
            pending_replies,
        }
    }

    fn next_message(&mut self) -> OutgoingMessage {
        self.message_rx
            .next()
            .now_or_never()
            .flatten()
            .expect("expected a queued message")
    }

    fn next_task(&mut self) -> Task {
        self.task_rx
            .next()
            .now_or_never()
            .flatten()
            .expect("expected a registered task")
    }

    fn route(
        &self,
        id: RequestId,
        result: Result<serde_json::Value, crate::Error>,
    ) -> Option<oneshot::Receiver<()>> {
        let pending_reply = self.pending_replies.remove(&id).unwrap();
        let (dispatch, response_dispatch) =
            incoming_actor::dispatch_from_response(id, pending_reply, result);
        let Dispatch::Response(result, router) = dispatch else {
            panic!("expected a response dispatch");
        };
        router.route_with_result(result).unwrap();
        response_dispatch.complete()
    }
}

fn request() -> UntypedMessage {
    UntypedMessage::new("prepared", json!({})).unwrap()
}

#[derive(Clone, Copy)]
enum Failure {
    Serialization,
    OutgoingClosed,
    IncomingClosed,
    DriverStopped,
}

#[derive(Clone, Debug)]
struct FailingRequest;

impl JsonRpcMessage for FailingRequest {
    fn matches_method(method: &str) -> bool {
        method == "failing"
    }
    fn method(&self) -> &'static str {
        "failing"
    }
    fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
        Err(crate::Error::invalid_params())
    }
    fn parse_message(_method: &str, _params: &impl Serialize) -> Result<Self, crate::Error> {
        Err(crate::Error::invalid_params())
    }
}

impl JsonRpcRequest for FailingRequest {
    type Response = serde_json::Value;
}
