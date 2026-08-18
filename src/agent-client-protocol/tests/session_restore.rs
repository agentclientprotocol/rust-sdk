//! Stable protocol v1 session restore: `session/load` and `session/resume`
//! (issue #323).
//!
//! v1 restore responses carry no session id — the id lives on the request —
//! so the SDK's restore path takes the id from the request, installs the
//! session routing *before* the request is published, and hands the complete
//! response back alongside the [`ActiveSession`]. These tests pin the two
//! properties that fall out of that design:
//!
//! - an early notification sharing a batch with the restore response is routed
//!   to the session even though it precedes the response in dispatch order;
//! - a failed restore drops the routing, so a later update for the failed
//!   session id cannot reach a stale handler.
//!
//! The raw-frame peers drive the exact transport wiring a real agent sees.

use std::time::Duration;

use agent_client_protocol::{
    Channel, Client, Error, RawJsonRpcMessage, SessionMessage, TransportBatch, TransportFrame,
    schema::v1::{
        ContentBlock, ContentChunk, LoadSessionRequest, LoadSessionResponse, ResumeSessionRequest,
        ResumeSessionResponse, SessionId, SessionMode, SessionModeId, SessionModeState,
        SessionNotification, SessionUpdate, TextContent,
    },
};
use futures::{StreamExt as _, channel::oneshot};

const TIMEOUT: Duration = Duration::from_secs(10);

fn modes_state() -> SessionModeState {
    SessionModeState::new(
        SessionModeId::new("default"),
        vec![SessionMode::new(SessionModeId::new("default"), "Default")],
    )
}

/// A `session/update` notification for `session_id`, serialized the way the
/// agent would put it on the wire.
fn update_notification(session_id: SessionId) -> RawJsonRpcMessage {
    RawJsonRpcMessage::notification(
        "session/update".to_string(),
        serde_json::to_value(SessionNotification::new(
            session_id,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("restored update"),
            ))),
        ))
        .expect("session notification should serialize"),
    )
    .expect("session notification should form valid JSON-RPC parameters")
}

/// The headline restore path: routing is installed before the request is
/// published, so an update that arrives *before* the restore response in the
/// same batch is still captured, and the complete load response round-trips.
#[tokio::test(flavor = "current_thread")]
async fn load_session_routes_early_notification_and_returns_exact_response() {
    let session_id = SessionId::new("restore-load");
    let response_session_id = session_id.clone();
    let peer_response_id = response_session_id.clone();
    let notification_session_id = session_id.clone();
    let expected = LoadSessionResponse::new().modes(modes_state()).meta(
        serde_json::json!({ "trace": "abc" })
            .as_object()
            .unwrap()
            .clone(),
    );
    let wire_response = expected.clone();
    let (transport, mut peer) = Channel::duplex();
    let (result_tx, result_rx) = oneshot::channel();

    let client = Client
        .builder()
        .connect_with(transport, async move |connection| {
            connection
                .load_session(session_id.clone(), "/restore/cwd")
                .on_session_start(async move |mut restored| {
                    // The notification was dispatched before the response; it
                    // must still reach the session's update stream.
                    let update = restored.session_mut().read_update().await?;
                    assert!(matches!(update, SessionMessage::SessionMessage(_)));
                    assert_eq!(restored.response(), &expected);
                    assert_eq!(restored.session().session_id(), &response_session_id);
                    result_tx.send(()).map_err(|()| Error::internal_error())
                })?;

            result_rx.await.map_err(|_| Error::internal_error())
        });

    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("expected a session/load request");
        };
        assert_eq!(request.method.as_ref(), "session/load");
        let req: LoadSessionRequest = serde_json::from_value(
            request
                .params
                .expect("session/load request carries params")
                .into_value(),
        )
        .expect("session/load params should parse");
        assert_eq!(req.session_id, peer_response_id);
        assert_eq!(req.cwd, std::path::PathBuf::from("/restore/cwd"));

        // Update BEFORE response in the same batch: with routing installed
        // before publish, dispatch can route it even though it precedes the
        // restore response.
        let notification = update_notification(notification_session_id);
        let response = RawJsonRpcMessage::response(
            request.id,
            Ok(serde_json::to_value(wire_response).expect("load response should serialize")),
        );
        let batch = TransportBatch::from_messages([notification, response])
            .expect("test batch should be non-empty");
        peer.tx
            .unbounded_send(TransportFrame::Batch(batch))
            .expect("client should accept the restore batch");

        while peer.rx.next().await.is_some() {}
        Ok::<(), Error>(())
    };

    tokio::time::timeout(TIMEOUT, async { futures::try_join!(client, peer) })
        .await
        .expect("same-batch restore update was not routed")
        .expect("load session connection failed");
}

/// The blocking path: `resume_session().block_task().start_session()` returns
/// the restored session and the exact resume response.
#[tokio::test(flavor = "current_thread")]
async fn resume_session_round_trips_exact_response() {
    let session_id = SessionId::new("restore-resume");
    let client_session_id = session_id.clone();
    let expected = ResumeSessionResponse::new().modes(modes_state());
    let wire_response = expected.clone();
    let (transport, mut peer) = Channel::duplex();

    let client = Client
        .builder()
        .connect_with(transport, async move |connection| {
            let restored = connection
                .resume_session(client_session_id.clone(), "/resume/cwd")
                .block_task()
                .start_session()
                .await?;
            assert_eq!(restored.session().session_id(), &client_session_id);
            assert_eq!(restored.response(), &expected);

            let (active, response) = restored.into_parts();
            assert_eq!(active.session_id(), &client_session_id);
            assert_eq!(response, expected);

            Ok(())
        });

    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("expected a session/resume request");
        };
        assert_eq!(request.method.as_ref(), "session/resume");
        let req: ResumeSessionRequest = serde_json::from_value(
            request
                .params
                .expect("session/resume request carries params")
                .into_value(),
        )
        .expect("session/resume params should parse");
        assert_eq!(req.session_id, session_id);

        let response = RawJsonRpcMessage::response(
            request.id,
            Ok(serde_json::to_value(wire_response).expect("resume response should serialize")),
        );
        peer.tx
            .unbounded_send(TransportFrame::Single(response))
            .expect("client should accept the resume response");

        while peer.rx.next().await.is_some() {}
        Ok::<(), Error>(())
    };

    tokio::time::timeout(TIMEOUT, async { futures::try_join!(client, peer) })
        .await
        .expect("resume session connection failed")
        .expect("resume session errored");
}

/// The interception entry point: a request built elsewhere (e.g. decoded from
/// a client's `session/load`) is handed to the SDK unchanged and forwarded
/// verbatim.
#[tokio::test(flavor = "current_thread")]
async fn restore_session_from_forwards_an_intercepted_request() {
    let session_id = SessionId::new("restore-from");
    let client_session_id = session_id.clone();
    let expected = LoadSessionResponse::new().modes(modes_state());
    let wire_response = expected.clone();
    let (transport, mut peer) = Channel::duplex();
    let (result_tx, result_rx) = oneshot::channel();

    let client = Client
        .builder()
        .connect_with(transport, async move |connection| {
            let request = LoadSessionRequest::new(client_session_id, "/intercepted/cwd");
            connection
                .restore_session_from(request)
                .on_session_start(async move |restored| {
                    assert_eq!(restored.response(), &expected);
                    result_tx.send(()).map_err(|()| Error::internal_error())
                })?;

            result_rx.await.map_err(|_| Error::internal_error())
        });

    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("expected a session/load request");
        };
        assert_eq!(request.method.as_ref(), "session/load");
        let req: LoadSessionRequest = serde_json::from_value(
            request
                .params
                .expect("session/load request carries params")
                .into_value(),
        )
        .expect("session/load params should parse");
        assert_eq!(req.session_id, session_id);
        assert_eq!(req.cwd, std::path::PathBuf::from("/intercepted/cwd"));

        let response = RawJsonRpcMessage::response(
            request.id,
            Ok(serde_json::to_value(wire_response).expect("load response should serialize")),
        );
        peer.tx
            .unbounded_send(TransportFrame::Single(response))
            .expect("client should accept the restore response");

        while peer.rx.next().await.is_some() {}
        Ok::<(), Error>(())
    };

    tokio::time::timeout(TIMEOUT, async { futures::try_join!(client, peer) })
        .await
        .expect("restore_session_from connection failed")
        .expect("restore_session_from errored");
}

/// A failed restore returns `Err` and drops the session routing: a subsequent
/// update for the failed session id must not reach a stale handler (an
/// `ActiveSessionHandler` whose receiver is gone would error dispatch), and
/// the connection must remain usable for a fresh restore.
#[tokio::test(flavor = "current_thread")]
async fn failed_restore_returns_err_and_drops_routing() {
    let failed_id = SessionId::new("restore-failed");
    let client_failed_id = failed_id.clone();
    let (transport, mut peer) = Channel::duplex();
    let (failed_tx, failed_rx) = oneshot::channel();
    let (proceed_tx, proceed_rx) = oneshot::channel();

    let client = Client
        .builder()
        .connect_with(transport, async move |connection| {
            let err = connection
                .load_session(client_failed_id, "/restore/fail")
                .block_task()
                .start_session()
                .await;
            assert!(err.is_err(), "an error response must fail the restore");

            // The failure consumed the guard; let the peer exercise the stale
            // handler question now.
            failed_tx.send(()).map_err(|()| Error::internal_error())?;
            proceed_rx.await.map_err(|_| Error::internal_error())?;

            // The connection is still healthy: a fresh restore succeeds, and
            // had the failed session's handler survived, the update the peer
            // sent would have errored dispatch before we got here.
            let recovered = SessionId::new("restore-recovery");
            let restored = connection
                .load_session(recovered.clone(), "/restore/ok")
                .block_task()
                .start_session()
                .await
                .expect("a second restore must succeed");
            assert_eq!(restored.session().session_id(), &recovered);
            Ok(())
        });

    let peer = async move {
        // 1. Fail the first restore.
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("expected the first session/load request");
        };
        assert_eq!(request.method.as_ref(), "session/load");
        peer.tx
            .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::response(
                request.id,
                Err(Error::internal_error().data("restore refused")),
            )))
            .expect("client should accept the error response");

        // 2. Once the failure has been consumed, send an update for the failed
        //    session id. With routing dropped it passes through unhandled.
        failed_rx.await.expect("client reported the failure");
        peer.tx
            .unbounded_send(TransportFrame::Single(update_notification(failed_id)))
            .expect("client should accept the late update");
        proceed_tx.send(()).map_err(|()| Error::internal_error())?;

        // 3. Serve the recovery restore.
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(recovery))) =
            peer.rx.next().await
        else {
            panic!("expected the second session/load request");
        };
        assert_eq!(recovery.method.as_ref(), "session/load");
        peer.tx
            .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::response(
                recovery.id,
                Ok(serde_json::to_value(LoadSessionResponse::new())
                    .expect("recovery response should serialize")),
            )))
            .expect("client should accept the recovery response");

        while peer.rx.next().await.is_some() {}
        Ok::<(), Error>(())
    };

    tokio::time::timeout(TIMEOUT, async { futures::try_join!(client, peer) })
        .await
        .expect("failed restore left the connection unhealthy")
        .expect("failed restore connection errored");
}
