//! Stable protocol v1 direct-client restore lifecycle (`session/load` and
//! `session/resume`).

use std::{future::pending, path::PathBuf, time::Duration};

use agent_client_protocol::{
    Agent, Channel, Client, ConnectionTo, Error, ErrorCode, JsonRpcMessage, JsonRpcNotification,
    RawJsonRpcMessage, Responder, SessionMessage, TransportBatch, TransportFrame, UntypedMessage,
    schema::v1::{
        CancelRequestNotification, ContentBlock, ContentChunk, LoadSessionRequest,
        LoadSessionResponse, RequestId, ResumeSessionRequest, ResumeSessionResponse,
        SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption, SessionId,
        SessionMode, SessionModeId, SessionModeState, SessionNotification, SessionUpdate,
        TextContent,
    },
};
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
    future::{self, Either},
};
use serde::{Deserialize, Serialize};

const TIMEOUT: Duration = Duration::from_secs(10);
const DISPATCH_BARRIER_METHOD: &str = "_test/session-restore-dispatch-barrier";

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcNotification)]
#[notification(method = "_test/session-restore-dispatch-barrier")]
struct DispatchBarrierNotification {}

fn modes_state() -> SessionModeState {
    SessionModeState::new(
        SessionModeId::new("default"),
        vec![SessionMode::new(SessionModeId::new("default"), "Default")],
    )
}

fn config_options() -> Vec<SessionConfigOption> {
    vec![
        SessionConfigOption::select(
            "thinking",
            "Thinking",
            "standard",
            vec![
                SessionConfigSelectOption::new("standard", "Standard"),
                SessionConfigSelectOption::new("extended", "Extended"),
            ],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]
}

fn meta(trace: &str) -> serde_json::Map<String, serde_json::Value> {
    serde_json::json!({ "trace": trace })
        .as_object()
        .expect("test metadata should be an object")
        .clone()
}

fn update_notification(session_id: SessionId, text: &str) -> RawJsonRpcMessage {
    RawJsonRpcMessage::notification(
        "session/update".to_owned(),
        serde_json::to_value(SessionNotification::new(
            session_id,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            ))),
        ))
        .expect("session notification should serialize"),
    )
    .expect("session notification should form valid JSON-RPC parameters")
}

fn dispatch_barrier_notification() -> RawJsonRpcMessage {
    RawJsonRpcMessage::notification(DISPATCH_BARRIER_METHOD.to_owned(), serde_json::json!({}))
        .expect("dispatch barrier should form valid JSON-RPC parameters")
}

fn session_message_text(message: SessionMessage) -> String {
    let SessionMessage::SessionMessage(dispatch) = message else {
        panic!("expected a session notification")
    };
    let untyped = dispatch
        .to_untyped_message()
        .expect("session dispatch should convert to an untyped message");
    let notification = SessionNotification::parse_message(untyped.method(), untyped.params())
        .expect("session notification should parse");
    let SessionUpdate::AgentMessageChunk(ContentChunk {
        content: ContentBlock::Text(text),
        ..
    }) = notification.update
    else {
        panic!("expected an agent text chunk")
    };
    text.text
}

#[tokio::test(flavor = "current_thread")]
async fn load_session_preserves_pre_response_replay_and_exact_response() {
    let session_id = SessionId::new("restore-load");
    let client_session_id = session_id.clone();
    let peer_session_id = session_id.clone();
    let expected = LoadSessionResponse::new()
        .modes(modes_state())
        .config_options(config_options())
        .meta(meta("load"));
    let wire_response = expected.clone();
    let (transport, mut peer) = Channel::duplex();
    let (done_tx, done_rx) = oneshot::channel();

    let client = Client
        .builder()
        .connect_with(transport, async move |connection| {
            connection
                .load_session(client_session_id.clone(), "/restore/load")
                .on_session_start(async move |mut restored| {
                    assert_eq!(restored.session().session_id(), &client_session_id);
                    assert_eq!(restored.session().modes(), expected.modes.as_ref());
                    assert_eq!(
                        restored.session().config_options(),
                        expected.config_options.as_deref()
                    );
                    assert_eq!(restored.session().meta(), expected.meta.as_ref());
                    assert_eq!(restored.response(), &expected);
                    assert_eq!(
                        session_message_text(restored.session_mut().read_update().await?),
                        "first replay"
                    );
                    assert_eq!(
                        session_message_text(restored.session_mut().read_update().await?),
                        "second replay"
                    );
                    done_tx.send(()).map_err(|()| Error::internal_error())
                })?;

            done_rx.await.map_err(Error::into_internal_error)
        });

    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("expected session/load")
        };
        let parsed = LoadSessionRequest::parse_message(&request.method, &request.params)?;
        assert_eq!(parsed.session_id, peer_session_id);
        assert_eq!(parsed.cwd, PathBuf::from("/restore/load"));

        let batch = TransportBatch::from_messages([
            update_notification(parsed.session_id.clone(), "first replay"),
            update_notification(parsed.session_id, "second replay"),
            RawJsonRpcMessage::response(request.id, Ok(serde_json::to_value(wire_response)?)),
        ])
        .expect("restore batch should be non-empty");
        peer.tx
            .unbounded_send(TransportFrame::Batch(batch))
            .expect("client should accept replay and response");

        while peer.rx.next().await.is_some() {}
        Ok::<(), Error>(())
    };

    tokio::time::timeout(TIMEOUT, async { futures::try_join!(client, peer) })
        .await
        .expect("load restore timed out")
        .expect("load restore failed");
}

#[tokio::test(flavor = "current_thread")]
async fn resume_session_returns_exact_response_and_an_active_session() {
    let session_id = SessionId::new("restore-resume");
    let client_session_id = session_id.clone();
    let expected = ResumeSessionResponse::new()
        .modes(modes_state())
        .config_options(config_options())
        .meta(meta("resume"));
    let wire_response = expected.clone();
    let (transport, mut peer) = Channel::duplex();

    let client = Client
        .builder()
        .connect_with(transport, async move |connection| {
            let mut restored = connection
                .resume_session(client_session_id.clone(), "/restore/resume")
                .block_task()
                .start_session()
                .await?;
            assert_eq!(restored.session().session_id(), &client_session_id);
            assert_eq!(restored.session().modes(), expected.modes.as_ref());
            assert_eq!(
                restored.session().config_options(),
                expected.config_options.as_deref()
            );
            assert_eq!(restored.session().meta(), expected.meta.as_ref());
            assert_eq!(restored.response(), &expected);
            assert_eq!(
                session_message_text(restored.session_mut().read_update().await?),
                "resume update"
            );

            let (session, response) = restored.into_parts();
            assert_eq!(session.session_id(), &client_session_id);
            assert_eq!(response, expected);
            Ok(())
        });

    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("expected session/resume")
        };
        let parsed = ResumeSessionRequest::parse_message(&request.method, &request.params)?;
        assert_eq!(parsed.session_id, session_id);

        let batch = TransportBatch::from_messages([
            RawJsonRpcMessage::response(request.id, Ok(serde_json::to_value(wire_response)?)),
            update_notification(parsed.session_id, "resume update"),
        ])
        .expect("resume batch should be non-empty");
        peer.tx
            .unbounded_send(TransportFrame::Batch(batch))
            .expect("client should accept update and response");

        while peer.rx.next().await.is_some() {}
        Ok::<(), Error>(())
    };

    tokio::time::timeout(TIMEOUT, async { futures::try_join!(client, peer) })
        .await
        .expect("resume restore timed out")
        .expect("resume restore failed");
}

#[tokio::test(flavor = "current_thread")]
async fn resume_session_from_preserves_the_existing_request() {
    let session_id = SessionId::new("restore-from");
    let expected_request = ResumeSessionRequest::new(session_id.clone(), "/restore/from")
        .additional_directories(vec![PathBuf::from("/restore/extra")])
        .meta(meta("request"));
    let client_request = expected_request.clone();
    let peer_request = expected_request.clone();
    let (transport, mut peer) = Channel::duplex();

    let client = Client
        .builder()
        .connect_with(transport, async move |connection| {
            let restored = connection
                .resume_session_from(client_request)
                .block_task()
                .start_session()
                .await?;
            assert_eq!(restored.session().session_id(), &session_id);
            Ok(())
        });

    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("expected session/resume")
        };
        assert_eq!(
            ResumeSessionRequest::parse_message(&request.method, &request.params)?,
            peer_request
        );
        peer.tx
            .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::response(
                request.id,
                Ok(serde_json::to_value(ResumeSessionResponse::new())?),
            )))
            .expect("client should accept resume response");
        while peer.rx.next().await.is_some() {}
        Ok::<(), Error>(())
    };

    tokio::time::timeout(TIMEOUT, async { futures::try_join!(client, peer) })
        .await
        .expect("restore-from timed out")
        .expect("restore-from failed");
}

#[tokio::test(flavor = "current_thread")]
async fn restore_waits_for_routing_acknowledgment_before_publication() {
    let (handler_started_tx, mut handler_started_rx) = mpsc::unbounded();
    let client = Client.builder().on_receive_request(
        async move |_request: UntypedMessage,
                    _responder: Responder<serde_json::Value>,
                    _connection: ConnectionTo<Agent>| {
            handler_started_tx
                .unbounded_send(())
                .map_err(Error::into_internal_error)?;
            pending::<Result<(), Error>>().await
        },
        agent_client_protocol::on_receive_request!(),
    );
    let (transport, mut peer) = Channel::duplex();
    let (restore_called_tx, restore_called_rx) = oneshot::channel();
    let (checked_tx, checked_rx) = oneshot::channel();

    let client = client.connect_with(transport, async move |connection| {
        handler_started_rx
            .next()
            .await
            .ok_or_else(Error::internal_error)?;
        connection
            .load_session("routing-barrier", "/restore/barrier")
            .on_session_start(async |_restored| Ok(()))?;
        restore_called_tx
            .send(())
            .map_err(|()| Error::internal_error())?;
        checked_rx.await.map_err(Error::into_internal_error)
    });

    let peer = async move {
        peer.tx
            .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::request(
                "test/block-incoming".to_owned(),
                serde_json::json!({}),
                RequestId::Number(1),
            )?))
            .expect("client should accept the blocking request");
        restore_called_rx
            .await
            .map_err(Error::into_internal_error)?;

        assert!(
            tokio::time::timeout(Duration::from_millis(200), peer.rx.next())
                .await
                .is_err(),
            "restore request was published before the incoming actor acknowledged its route"
        );
        checked_tx.send(()).map_err(|()| Error::internal_error())?;
        Ok::<(), Error>(())
    };

    tokio::time::timeout(TIMEOUT, async { futures::try_join!(client, peer) })
        .await
        .expect("routing-barrier test timed out")
        .expect("routing-barrier test failed");
}

#[tokio::test(flavor = "current_thread")]
async fn failed_restore_removes_routing_before_later_batch_entries() {
    let session_id = SessionId::new("restore-failed");
    let client_session_id = session_id.clone();
    let (transport, mut peer) = Channel::duplex();
    let (barrier_tx, mut barrier_rx) = mpsc::unbounded();

    let client = Client.builder().on_receive_notification(
        async move |_notification: DispatchBarrierNotification,
                    _connection: ConnectionTo<Agent>| {
            barrier_tx
                .unbounded_send(())
                .map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );

    let client = client.connect_with(transport, async move |connection| {
        let error = connection
            .load_session(client_session_id.clone(), "/restore/fail")
            .block_task()
            .start_session()
            .await
            .expect_err("agent should reject session/load");
        assert_eq!(error.code, ErrorCode::InvalidParams);
        barrier_rx.next().await.ok_or_else(Error::internal_error)?;

        let mut restored = connection
            .load_session(client_session_id, "/restore/retry")
            .block_task()
            .start_session()
            .await?;
        assert_eq!(
            session_message_text(restored.session_mut().read_update().await?),
            "after failed restore"
        );
        Ok(())
    });

    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("expected session/load")
        };
        let batch = TransportBatch::from_messages([
            RawJsonRpcMessage::response(
                request.id,
                Err(Error::invalid_params().data("restore refused")),
            ),
            update_notification(session_id, "after failed restore"),
            dispatch_barrier_notification(),
        ])
        .expect("failure batch should be non-empty");
        peer.tx
            .unbounded_send(TransportFrame::Batch(batch))
            .expect("client should accept failure, probe, and barrier");

        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(retry))) = peer.rx.next().await
        else {
            panic!("expected retry session/load")
        };
        peer.tx
            .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::response(
                retry.id,
                Ok(serde_json::to_value(LoadSessionResponse::new())?),
            )))
            .expect("client should accept retry response");
        while peer.rx.next().await.is_some() {}
        Ok::<(), Error>(())
    };

    tokio::time::timeout(TIMEOUT, async { futures::try_join!(client, peer) })
        .await
        .expect("failure cleanup timed out")
        .expect("failure cleanup failed");
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_restore_cancels_request_and_removes_routing() {
    let session_id = SessionId::new("restore-cancelled");
    let client_session_id = session_id.clone();
    let (transport, mut peer) = Channel::duplex();
    let (request_seen_tx, request_seen_rx) = oneshot::channel();
    let (future_dropped_tx, future_dropped_rx) = oneshot::channel();
    let (barrier_tx, mut barrier_rx) = mpsc::unbounded();
    let (barrier_observed_tx, barrier_observed_rx) = oneshot::channel();

    let client = Client.builder().on_receive_notification(
        async move |_notification: DispatchBarrierNotification,
                    _connection: ConnectionTo<Agent>| {
            barrier_tx
                .unbounded_send(())
                .map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );

    let client = client.connect_with(transport, async move |connection| {
        let pending_restore = Box::pin(
            connection
                .resume_session(client_session_id.clone(), "/restore/cancel")
                .block_task()
                .start_session(),
        );

        match future::select(pending_restore, request_seen_rx).await {
            Either::Right((seen, pending_restore)) => {
                seen.map_err(Error::into_internal_error)?;
                drop(pending_restore);
            }
            Either::Left((result, _)) => {
                panic!("restore completed before cancellation: {result:?}")
            }
        }
        future_dropped_tx
            .send(())
            .map_err(|()| Error::internal_error())?;
        barrier_rx.next().await.ok_or_else(Error::internal_error)?;
        barrier_observed_tx
            .send(())
            .map_err(|()| Error::internal_error())?;

        let mut restored = connection
            .resume_session(client_session_id, "/restore/after-cancel")
            .block_task()
            .start_session()
            .await?;
        assert_eq!(
            session_message_text(restored.session_mut().read_update().await?),
            "after cancelled restore"
        );
        Ok(())
    });

    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("expected session/resume")
        };
        let restore_request_id = request.id.clone();
        request_seen_tx
            .send(())
            .map_err(|()| Error::internal_error())?;
        future_dropped_rx
            .await
            .map_err(Error::into_internal_error)?;

        let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(notification))) =
            peer.rx.next().await
        else {
            panic!("dropping the restore future should send $/cancel_request")
        };
        let cancellation =
            CancelRequestNotification::parse_message(&notification.method, &notification.params)?;
        assert_eq!(cancellation.request_id, restore_request_id);

        let probe_batch = TransportBatch::from_messages([
            update_notification(session_id, "after cancelled restore"),
            dispatch_barrier_notification(),
        ])
        .expect("cancellation probe batch should be non-empty");
        peer.tx
            .unbounded_send(TransportFrame::Batch(probe_batch))
            .expect("client should accept cancellation probe and barrier");
        barrier_observed_rx
            .await
            .map_err(Error::into_internal_error)?;

        peer.tx
            .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::response(
                request.id,
                Err(Error::request_cancelled()),
            )))
            .expect("client should accept the cancelled request's response");

        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(retry))) = peer.rx.next().await
        else {
            panic!("expected retry session/resume")
        };
        peer.tx
            .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::response(
                retry.id,
                Ok(serde_json::to_value(ResumeSessionResponse::new())?),
            )))
            .expect("client should accept retry response");
        while peer.rx.next().await.is_some() {}
        Ok::<(), Error>(())
    };

    tokio::time::timeout(TIMEOUT, async { futures::try_join!(client, peer) })
        .await
        .expect("cancellation cleanup timed out")
        .expect("cancellation cleanup failed");
}
