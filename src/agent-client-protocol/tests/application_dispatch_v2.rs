#![cfg(feature = "unstable_protocol_v2")]

use std::{cell::RefCell, rc::Rc, time::Duration};

use agent_client_protocol::{
    Agent, Channel, Client, Error, RawJsonRpcMessage, TransportBatch, TransportFrame,
    V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use futures::{StreamExt as _, channel::mpsc};
use serde_json::json;

#[derive(Debug)]
enum ApplicationEvent {
    Update(Box<v2::UpdateSessionNotification>),
    ResumeFinished(Result<v2::ResumeSessionResponse, Error>),
    Closed,
}

#[tokio::test(flavor = "current_thread")]
async fn application_queue_orders_replay_response_and_eof() {
    assert_application_order(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn application_queue_orders_batched_replay_response_and_eof() {
    assert_application_order(true).await;
}

async fn assert_application_order(batched: bool) {
    let (transport, mut peer) = Channel::duplex();
    let (events_tx, mut events_rx) = mpsc::unbounded();
    let updates_tx = events_tx.clone();
    let closed_tx = events_tx.clone();

    // Deliberately !Send application state: only the foreground consumer may
    // touch it, not the Send callbacks installed in the SDK.
    let applied = Rc::new(RefCell::new(Vec::new()));
    let observed = applied.clone();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: V2ConnectionTo<Agent>| {
                updates_tx
                    .unbounded_send(ApplicationEvent::Update(Box::new(update)))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_close(async move |_connection| {
            drop(closed_tx.unbounded_send(ApplicationEvent::Closed));
            Ok(())
        })
        .connect_with(transport, async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("ordered-client", "0.1.0"),
                ))
                .block_task()
                .await?;

            let request = v2::ResumeSessionRequest::new(
                "resumed-session",
                std::env::current_dir().map_err(Error::into_internal_error)?,
            )
            .replay_from(v2::ReplayFrom::from(v2::ReplayFromStart::new()));
            connection
                .send_request(request)
                .on_receiving_result(async move |result| {
                    events_tx
                        .unbounded_send(ApplicationEvent::ResumeFinished(result))
                        .map_err(Error::into_internal_error)
                })?;

            // Simulate a stalled UI. Dispatch, the response callback, and EOF
            // must progress without waiting for the application to drain.
            connection.incoming_closed().await;
            assert!(applied.borrow().is_empty());

            let mut resumed = false;
            loop {
                match events_rx.next().await.expect("expected application event") {
                    ApplicationEvent::Update(notification) => {
                        assert_eq!(
                            notification.session_id,
                            v2::SessionId::new("resumed-session")
                        );
                        let v2::SessionUpdate::AgentMessage(message) = notification.update else {
                            panic!("expected an agent message");
                        };
                        applied.borrow_mut().push(message.message_id.to_string());
                    }
                    ApplicationEvent::ResumeFinished(result) => {
                        assert!(!resumed, "the resume result must be exposed only once");
                        assert_eq!(
                            *applied.borrow(),
                            ["replayed-first", "replayed-second"],
                            "replay must be applied before exposing the resume result, \
                             without applying the later live update first"
                        );
                        assert_eq!(result?, v2::ResumeSessionResponse::new());
                        resumed = true;
                    }
                    ApplicationEvent::Closed => {
                        assert!(resumed, "EOF must not overtake the wire response");
                        assert_eq!(
                            *applied.borrow(),
                            ["replayed-first", "replayed-second", "live"],
                            "EOF must not overtake already-received updates"
                        );
                        break;
                    }
                }
            }
            Ok(())
        });

    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(initialize))) =
            peer.rx.next().await
        else {
            panic!("expected initialize");
        };
        assert_eq!(initialize.method.as_ref(), "initialize");
        peer.tx
            .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::response(
                initialize.id,
                Ok(serde_json::to_value(
                    v2::InitializeResponse::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("ordered-agent", "0.1.0"),
                    )
                    .capabilities(
                        v2::AgentCapabilities::new().session(v2::SessionCapabilities::new()),
                    ),
                )
                .unwrap()),
            )))
            .unwrap();

        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(resume))) = peer.rx.next().await
        else {
            panic!("expected resume");
        };
        assert_eq!(resume.method.as_ref(), "session/resume");

        let update = |message_id| {
            RawJsonRpcMessage::notification(
                "session/update".into(),
                json!({
                    "sessionId": "resumed-session",
                    "update": {
                        "sessionUpdate": "agent_message",
                        "messageId": message_id,
                        "content": [{"type": "text", "text": message_id}]
                    }
                }),
            )
            .unwrap()
        };
        let messages = [
            update("replayed-first"),
            update("replayed-second"),
            RawJsonRpcMessage::response(resume.id, Ok(json!({}))),
            update("live"),
        ];
        if batched {
            peer.tx
                .unbounded_send(TransportFrame::Batch(
                    TransportBatch::from_messages(messages).unwrap(),
                ))
                .unwrap();
        } else {
            for message in messages {
                peer.tx
                    .unbounded_send(TransportFrame::Single(message))
                    .unwrap();
            }
        }
        // EOF is immediately behind the queued traffic, not delayed until the
        // application has consumed the response or any updates.
        drop(peer.tx);
        while peer.rx.next().await.is_some() {}
    };

    tokio::time::timeout(Duration::from_secs(10), async {
        let (result, ()) = futures::join!(client, peer);
        result
    })
    .await
    .expect("application dispatch test timed out")
    .expect("application dispatch failed");
    assert_eq!(
        *observed.borrow(),
        ["replayed-first", "replayed-second", "live"]
    );
}
