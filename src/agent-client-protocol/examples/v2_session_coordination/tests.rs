use std::{future::Future, time::Duration};

use agent_client_protocol::{
    Channel, RawJsonRpcMessage, TransportBatch, TransportFrame, schema::v1::RequestId,
};
use serde_json::{Value, json};

use super::*;

const SESSION: &str = "session";
const OTHER: &str = "other";

struct Peer(Channel);

impl Peer {
    async fn request(&mut self, method: &str, session: Option<&str>) -> RequestId {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            self.0.rx.next().await
        else {
            panic!("expected {method}");
        };
        assert_eq!(request.method.as_ref(), method);
        let params = serde_json::to_value(request.params).unwrap();
        if let Some(session) = session {
            assert_eq!(params["sessionId"], session);
        }
        if method == "session/resume" {
            assert_eq!(params["replayFrom"], json!({"type": "start"}));
            assert!(PathBuf::from(params["cwd"].as_str().unwrap()).is_absolute());
        }
        request.id
    }

    fn respond(&self, id: RequestId, result: Result<Value, Error>) {
        self.0
            .tx
            .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::response(
                id, result,
            )))
            .unwrap();
    }

    fn replay_and_respond(&self, id: RequestId, session: &str, text: &str) {
        self.0
            .tx
            .unbounded_send(TransportFrame::Batch(
                TransportBatch::from_messages([
                    update(session, text),
                    RawJsonRpcMessage::response(id, Ok(json!({}))),
                ])
                .unwrap(),
            ))
            .unwrap();
    }

    async fn initialize(&mut self) {
        let id = self.request("initialize", None).await;
        self.respond(
            id,
            Ok(serde_json::to_value(
                v2::InitializeResponse::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("gated-agent", "0.1.0"),
                )
                .capabilities(v2::AgentCapabilities::new().session(v2::SessionCapabilities::new())),
            )
            .unwrap()),
        );
    }

    async fn finish(mut self, sessions: &[&str], done: oneshot::Sender<()>) {
        for session in sessions {
            let close = self.request("session/close", Some(session)).await;
            self.respond(close, Ok(json!({})));
        }
        done.send(()).unwrap();
        self.shutdown().await;
    }

    async fn shutdown(&mut self) {
        // Keep the peer's incoming sender alive until the client stops. The
        // coordinator must not depend on the peer initiating disconnect.
        while let Some(frame) = self.0.rx.next().await {
            assert!(
                matches!(
                    frame,
                    TransportFrame::Single(RawJsonRpcMessage::Notification(_))
                ),
                "unexpected request during shutdown: {frame:?}"
            );
        }
    }
}

fn update(session: &str, text: &str) -> RawJsonRpcMessage {
    RawJsonRpcMessage::notification(
        "session/update".into(),
        json!({
            "sessionId": session,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "message",
                "content": {"type": "text", "text": text}
            }
        }),
    )
    .unwrap()
}

fn text(session: &Session) -> String {
    session
        .view
        .borrow()
        .updates
        .iter()
        .map(|update| {
            let v2::SessionUpdate::AgentMessageChunk(chunk) = update else {
                panic!("expected a chunk");
            };
            let v2::ContentBlock::Text(text) = &chunk.content else {
                panic!("expected text");
            };
            text.text.as_str()
        })
        .collect()
}

async fn exercise(
    application: impl AsyncFnOnce(Sessions) -> Result<(), Error>,
    peer: impl AsyncFnOnce(Peer),
) {
    let (transport, channel) = Channel::duplex();
    let client = with_sessions(transport, std::env::current_dir().unwrap(), application);
    bounded(async {
        let (result, ()) = futures::join!(client, async {
            let mut channel = Peer(channel);
            channel.initialize().await;
            peer(channel).await;
        });
        result.expect("coordinator connection failed");
    })
    .await;
}

async fn bounded(future: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("session coordination stalled");
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_loaders_share_one_resume_and_projection() {
    let expected = v2::ResumeSessionResponse::new()
        .config_options(vec![v2::SessionConfigOption::boolean(
            "thinking", "Thinking", true,
        )])
        .meta(serde_json::Map::from_iter([(
            "opaque".into(),
            json!({"kept": true}),
        )]));
    let response = serde_json::to_value(&expected).unwrap();
    let (started, start) = oneshot::channel();
    let (abandoned, abandon) = oneshot::channel();
    let (done, finished) = oneshot::channel();
    exercise(
        async move |sessions| {
            let first = sessions.load(SESSION.into())?;
            let second = sessions.load(SESSION.into())?;
            let third = sessions.load(SESSION.into())?;
            start.await.unwrap();
            drop(first);
            abandoned.send(()).unwrap();
            let (second, third) = futures::try_join!(second.wait(), third.wait())?;
            assert!(Rc::ptr_eq(&second.view, &third.view));
            assert_eq!(text(&second), "hello");
            assert_eq!(second.view.borrow().response, expected);

            let clone = second.clone();
            drop(second);
            let joined = sessions.load(SESSION.into())?.wait().await?;
            assert!(Rc::ptr_eq(&clone.view, &joined.view));
            let view = Rc::downgrade(&clone.view);
            drop((clone, third, joined));
            assert!(view.upgrade().is_none(), "cleanup must not retain the view");
            finished.await.unwrap();
            Ok(())
        },
        async move |mut peer| {
            let resume = peer.request("session/resume", Some(SESSION)).await;
            started.send(()).unwrap();
            abandon.await.unwrap();
            peer.0
                .tx
                .unbounded_send(TransportFrame::Single(update(SESSION, "hello")))
                .unwrap();
            peer.respond(resume, Ok(response));
            // A second resume or an early/duplicate close fails this script.
            peer.finish(&[SESSION], done).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn abandoned_resume_is_drained_and_closed_before_fresh_replay() {
    let (started, start) = oneshot::channel();
    let (replaced, replace) = oneshot::channel();
    let (done, finished) = oneshot::channel();
    exercise(
        async move |sessions| {
            let old = sessions.load(SESSION.into())?;
            start.await.unwrap();
            drop(old);
            let replacement = sessions.load(SESSION.into())?;
            replaced.send(()).unwrap();
            let session = replacement.wait().await?;
            assert_eq!(text(&session), "hello", "must not mix two replay streams");
            drop(session);
            finished.await.unwrap();
            Ok(())
        },
        async move |mut peer| {
            let old = peer.request("session/resume", Some(SESSION)).await;
            started.send(()).unwrap();
            replace.await.unwrap();
            peer.replay_and_respond(old, SESSION, "hello");
            let close = peer.request("session/close", Some(SESSION)).await;
            // Pre-close traffic must also drain before installing a new recipient.
            peer.0
                .tx
                .unbounded_send(TransportFrame::Batch(
                    TransportBatch::from_messages([
                        update(SESSION, "closing"),
                        RawJsonRpcMessage::response(close, Ok(json!({}))),
                    ])
                    .unwrap(),
                ))
                .unwrap();
            let fresh = peer.request("session/resume", Some(SESSION)).await;
            peer.replay_and_respond(fresh, SESSION, "hello");
            peer.finish(&[SESSION], done).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn delayed_close_blocks_only_its_session() {
    let (closing, close_started) = oneshot::channel();
    let (other_loaded, release_close) = oneshot::channel();
    let (done, finished) = oneshot::channel();
    exercise(
        async move |sessions| {
            let first = sessions.load(SESSION.into())?.wait().await?;
            drop(first);
            close_started.await.unwrap();
            let reopened = sessions.load(SESSION.into())?;
            let other = sessions.load(OTHER.into())?.wait().await?;
            other_loaded.send(()).unwrap();
            let reopened = reopened.wait().await?;
            assert_eq!(text(&reopened), "fresh");
            assert_eq!(text(&other), "other+live");
            drop(reopened);
            drop(other);
            finished.await.unwrap();
            Ok(())
        },
        async move |mut peer| {
            let first = peer.request("session/resume", Some(SESSION)).await;
            peer.replay_and_respond(first, SESSION, "old");
            let close = peer.request("session/close", Some(SESSION)).await;
            closing.send(()).unwrap();
            // An independent resume is a positive progress fence, not a sleep
            // or a racy "no request available yet" assertion.
            let other = peer.request("session/resume", Some(OTHER)).await;
            peer.replay_and_respond(other, OTHER, "other");
            release_close.await.unwrap();
            peer.0
                .tx
                .unbounded_send(TransportFrame::Batch(
                    TransportBatch::from_messages([
                        update(OTHER, "+live"),
                        RawJsonRpcMessage::response(close, Ok(json!({}))),
                    ])
                    .unwrap(),
                ))
                .unwrap();
            let fresh = peer.request("session/resume", Some(SESSION)).await;
            peer.replay_and_respond(fresh, SESSION, "fresh");
            peer.finish(&[SESSION, OTHER], done).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn unpolled_abandonment_does_not_publish_a_resume() {
    let (done, finished) = oneshot::channel();
    exercise(
        async move |sessions| {
            let unpolled = sessions.load(SESSION.into())?.wait();
            drop(unpolled);
            let other = sessions.load(OTHER.into())?.wait().await?;
            drop(other);
            finished.await.unwrap();
            Ok(())
        },
        async move |mut peer| {
            let resume = peer.request("session/resume", Some(OTHER)).await;
            peer.respond(resume, Ok(json!({})));
            peer.finish(&[OTHER], done).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn abandonment_after_response_delivery_still_closes_before_reopening() {
    let (done, finished) = oneshot::channel();
    exercise(
        async move |sessions| {
            let unconsumed = sessions.load(SESSION.into())?;
            let witness = sessions.load(SESSION.into())?.wait().await?;
            // Both replies have been delivered by the single driver turn, but
            // the first caller has not consumed its reply.
            let view = Rc::downgrade(&witness.view);
            drop(witness);
            drop(unconsumed);
            assert!(view.upgrade().is_none());
            let replacement = sessions.load(SESSION.into())?.wait().await?;
            assert_eq!(text(&replacement), "fresh");
            drop(replacement);
            finished.await.unwrap();
            Ok(())
        },
        async move |mut peer| {
            let old = peer.request("session/resume", Some(SESSION)).await;
            peer.replay_and_respond(old, SESSION, "old");
            let close = peer.request("session/close", Some(SESSION)).await;
            peer.respond(close, Ok(json!({})));
            let fresh = peer.request("session/resume", Some(SESSION)).await;
            peer.replay_and_respond(fresh, SESSION, "fresh");
            peer.finish(&[SESSION], done).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn disconnect_during_cleanup_fails_waiting_reopen() {
    let (started, start) = oneshot::channel();
    let (replaced, replace) = oneshot::channel();
    exercise(
        async move |sessions| {
            let old = sessions.load(SESSION.into())?;
            start.await.unwrap();
            drop(old);
            let replacement = sessions.load(SESSION.into())?;
            replaced.send(()).unwrap();
            assert!(replacement.wait().await.is_err());
            Ok(())
        },
        async move |mut peer| {
            let old = peer.request("session/resume", Some(SESSION)).await;
            started.send(()).unwrap();
            replace.await.unwrap();
            peer.respond(old, Ok(json!({})));
            peer.request("session/close", Some(SESSION)).await;
            drop(peer.0.tx);
            // A replacement resume must never have been published.
            while let Some(frame) = peer.0.rx.next().await {
                assert!(matches!(
                    frame,
                    TransportFrame::Single(RawJsonRpcMessage::Notification(_))
                ));
            }
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn last_owner_release_stops_transport_with_resume_unanswered() {
    let (started, start) = oneshot::channel();
    exercise(
        async move |sessions| {
            let load = sessions.load(SESSION.into())?;
            start.await.unwrap();
            drop((load, sessions));
            Ok(())
        },
        async move |mut peer| {
            peer.request("session/resume", Some(SESSION)).await;
            started.send(()).unwrap();
            // No response and no peer EOF. Still must observe client shutdown.
            peer.shutdown().await;
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn failed_close_blocks_reopening_but_not_other_sessions() {
    let (done, finished) = oneshot::channel();
    exercise(
        async move |sessions| {
            let first = sessions.load(SESSION.into())?.wait().await?;
            drop(first);
            assert!(sessions.load(SESSION.into())?.wait().await.is_err());
            // Retrying must fail locally rather than pretending cleanup succeeded.
            assert!(sessions.load(SESSION.into())?.wait().await.is_err());
            let other = sessions.load(OTHER.into())?.wait().await?;
            drop(other);
            finished.await.unwrap();
            Ok(())
        },
        async move |mut peer| {
            let first = peer.request("session/resume", Some(SESSION)).await;
            peer.respond(first, Ok(json!({})));
            let close = peer.request("session/close", Some(SESSION)).await;
            peer.respond(close, Err(Error::internal_error().data("cleanup failed")));
            let other = peer.request("session/resume", Some(OTHER)).await;
            peer.respond(other, Ok(json!({})));
            peer.finish(&[OTHER], done).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn failed_resume_releases_all_waiters_and_allows_retry() {
    let (done, finished) = oneshot::channel();
    exercise(
        async move |sessions| {
            let first = sessions.load(SESSION.into())?;
            let second = sessions.load(SESSION.into())?;
            let (first, second) = futures::join!(first.wait(), second.wait());
            assert!(first.is_err());
            assert!(second.is_err());
            let retry = sessions.load(SESSION.into())?.wait().await?;
            drop(retry);
            finished.await.unwrap();
            Ok(())
        },
        async move |mut peer| {
            let failed = peer.request("session/resume", Some(SESSION)).await;
            peer.respond(failed, Err(Error::invalid_params().data("try again")));
            // A rejected resume needs no close.
            let retry = peer.request("session/resume", Some(SESSION)).await;
            peer.respond(retry, Ok(json!({})));
            peer.finish(&[SESSION], done).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn stale_releases_do_not_affect_a_new_operation() {
    let (retry_started, retry_start) = oneshot::channel();
    let (released, release) = oneshot::channel();
    let (done, finished) = oneshot::channel();
    exercise(
        async move |sessions| {
            let old_pending = sessions.load(SESSION.into())?;
            let old_ready = sessions.load(SESSION.into())?;
            assert!(sessions.load(SESSION.into())?.wait().await.is_err());

            let retry = sessions.load(SESSION.into())?;
            retry_start.await.unwrap();
            drop(old_pending);
            released.send(()).unwrap();
            let retry = retry.wait().await?;
            assert_eq!(text(&retry), "fresh");
            drop(old_ready);
            let joined = sessions.load(SESSION.into())?.wait().await?;
            assert!(Rc::ptr_eq(&retry.view, &joined.view));
            drop((retry, joined));
            finished.await.unwrap();
            Ok(())
        },
        async move |mut peer| {
            let failed = peer.request("session/resume", Some(SESSION)).await;
            peer.respond(failed, Err(Error::invalid_params()));
            let retry = peer.request("session/resume", Some(SESSION)).await;
            retry_started.send(()).unwrap();
            release.await.unwrap();
            peer.replay_and_respond(retry, SESSION, "fresh");
            peer.finish(&[SESSION], done).await;
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn eof_follows_received_replay_and_response_but_fails_unanswered_loads() {
    let retained = Rc::new(RefCell::new(None));
    let result = retained.clone();
    exercise(
        async move |sessions| {
            let answered = sessions.load(SESSION.into())?;
            let unanswered = sessions.load(OTHER.into())?;
            let (answered, unanswered) = futures::join!(answered.wait(), unanswered.wait());
            assert!(unanswered.is_err());
            *retained.borrow_mut() = Some((answered?, sessions));
            Ok(())
        },
        async move |mut peer| {
            let answered = peer.request("session/resume", Some(SESSION)).await;
            peer.request("session/resume", Some(OTHER)).await;
            peer.replay_and_respond(answered, SESSION, "before EOF");
            drop(peer.0.tx);
            while let Some(frame) = peer.0.rx.next().await {
                assert!(matches!(
                    frame,
                    TransportFrame::Single(RawJsonRpcMessage::Notification(_))
                ));
            }
        },
    )
    .await;
    let (session, sessions) = result.borrow_mut().take().unwrap();
    assert_eq!(text(&session), "before EOF");
    assert!(session.view.borrow().disconnected);
    assert!(sessions.load(SESSION.into()).is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn last_owner_release_stops_transport_with_close_unanswered() {
    let (closing, close_started) = oneshot::channel();
    exercise(
        async move |sessions| {
            let session = sessions.load(SESSION.into())?.wait().await?;
            drop(session);
            close_started.await.unwrap();
            drop(sessions);
            Ok(())
        },
        async move |mut peer| {
            let resume = peer.request("session/resume", Some(SESSION)).await;
            peer.respond(resume, Ok(json!({})));
            peer.request("session/close", Some(SESSION)).await;
            closing.send(()).unwrap();
            peer.shutdown().await;
        },
    )
    .await;
}
