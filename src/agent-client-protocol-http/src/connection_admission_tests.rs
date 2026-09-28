use agent_client_protocol::ConnectionLimits;
use serde_json::json;

use super::*;

#[tokio::test]
async fn outbound_lease_releases_delivered_frame_before_waiting_or_idle_poll() {
    let frame = TransportFrame::Single(
        RawJsonRpcMessage::notification("test/data".into(), json!({"data": "x".repeat(100)}))
            .unwrap(),
    );
    let bytes = frame.to_json().unwrap().len();
    let limits = ConnectionLimits {
        max_frame_bytes: bytes + 1,
        max_queued_bytes: bytes * 2 + 1,
        max_queued_frames: 2,
    };
    let (_, channel) = Channel::duplex_with_limits(limits);
    let admission = channel.tx.admission();
    let mailbox = OutboundMailbox::new();
    let mut lease = mailbox.try_acquire().unwrap();
    let (_, first) = admission.try_admit(frame.clone()).unwrap().into_parts();
    mailbox
        .push_with_permit("first".into(), Some(first))
        .unwrap();
    assert_eq!(lease.recv().await.as_deref(), Some("first"));
    assert!(admission.try_admit(frame.clone()).is_err());
    assert!(lease.try_recv().is_err());
    let (_, second) = admission.try_admit(frame.clone()).unwrap().into_parts();
    mailbox
        .push_with_permit("second".into(), Some(second))
        .unwrap();
    assert_eq!(lease.try_recv().unwrap(), "second");
    assert!(admission.try_admit(frame.clone()).is_err());
    let wait = tokio::spawn(async move { lease.recv().await });
    tokio::task::yield_now().await;
    let (_, third) = admission.try_admit(frame).unwrap().into_parts();
    mailbox
        .push_with_permit("third".into(), Some(third))
        .unwrap();
    assert_eq!(wait.await.unwrap().as_deref(), Some("third"));
}

#[tokio::test]
async fn session_key_does_not_pin_unrelated_post_payload() {
    let frame = TransportFrame::Single(
        RawJsonRpcMessage::notification(
            "session/update".into(),
            json!({"sessionId": "persisted", "payload": "x".repeat(512)}),
        )
        .unwrap(),
    );
    let bytes = frame.to_json().unwrap().len();
    let limits = ConnectionLimits {
        max_frame_bytes: bytes,
        max_queued_bytes: bytes * 2 + 64,
        max_queued_frames: 2,
    };
    let (_, channel) = Channel::duplex_with_limits(limits);
    let admission = channel.tx.admission();
    let (_, source) = admission.try_admit(frame.clone()).unwrap().into_parts();
    let mut http = HttpOutbound::new();
    http.limits = limits;
    http.register_post_routes(&["persisted".into()], &[], &source)
        .await
        .unwrap();
    drop(source);
    assert!(http.session_streams.read().await.contains_key("persisted"));
    assert!(
        admission.try_admit(frame).is_ok(),
        "the retained session key must not pin its source payload"
    );
}

#[tokio::test]
async fn failed_session_key_reservation_does_not_publish_partial_batch() {
    let frame = TransportFrame::Single(
        RawJsonRpcMessage::notification("test/data".into(), json!({"payload": "x".repeat(512)}))
            .unwrap(),
    );
    let bytes = frame.to_json().unwrap().len();
    let limits = ConnectionLimits {
        max_frame_bytes: bytes,
        max_queued_bytes: bytes * 2 + 12,
        max_queued_frames: 3,
    };
    let (_, channel) = Channel::duplex_with_limits(limits);
    let (_, source) = channel
        .tx
        .admission()
        .try_admit(frame)
        .unwrap()
        .into_parts();
    let mut http = HttpOutbound::new();
    http.limits = limits;
    assert!(
        http.register_post_routes(&["one".into(), "another".into()], &[], &source)
            .await
            .is_err()
    );
    assert!(http.session_streams.read().await.is_empty());
    assert!(http.pending_routes.lock().await.is_empty());
}

#[tokio::test]
async fn route_and_session_metadata_admission_is_atomic_and_releases_permits() {
    let frame = TransportFrame::Single(
        RawJsonRpcMessage::request("test/request".into(), json!({}), RequestId::Number(1)).unwrap(),
    );
    let bytes = frame.to_json().unwrap().len();
    let limits = ConnectionLimits {
        max_frame_bytes: bytes + 16,
        max_queued_bytes: bytes * 2 + 32,
        max_queued_frames: 2,
    };
    let (_caller, transport) = Channel::duplex_with_limits(limits);
    let admission = transport.tx.admission();
    let (_, permit) = admission.try_admit(frame.clone()).unwrap().into_parts();
    let mut http = HttpOutbound::new();
    http.limits = limits;
    assert!(
        OutboundTransport::Http(Box::new(HttpOutbound::new()))
            .subscribe_session_stream("unknown")
            .await
            .is_none()
    );
    let first = [(RequestId::Number(1), ResponseRoute::Session("one".into()))];
    http.register_post_routes(&["one".into()], &first, &permit)
        .await
        .unwrap();
    let extra = [(RequestId::Number(2), ResponseRoute::Session("two".into()))];
    assert!(
        http.register_post_routes(&["two".into()], &extra, &permit)
            .await
            .is_err()
    );
    assert_eq!(http.session_streams.read().await.len(), 1);
    assert_eq!(http.pending_routes.lock().await.len(), 1);
    drop(permit);
    assert!(admission.try_admit(frame.clone()).is_err());
    assert_eq!(
        take_pending_route(
            &mut *http.pending_routes.lock().await,
            &RequestId::Number(1)
        ),
        Some(ResponseRoute::Session("one".into()))
    );
    assert!(
        admission.try_admit(frame.clone()).is_ok(),
        "removing a pending route releases its full source-frame charge"
    );
    http.session_streams.write().await.clear();
    assert!(admission.try_admit(frame).is_ok());
}

#[tokio::test]
async fn rolling_back_rejected_transport_send_removes_only_new_metadata() {
    let frame = TransportFrame::Single(
        RawJsonRpcMessage::request("test/request".into(), json!({}), RequestId::Number(1)).unwrap(),
    );
    let (_caller, transport) = Channel::duplex();
    let (_, permit) = transport
        .tx
        .admission()
        .try_admit(frame)
        .unwrap()
        .into_parts();
    let http = HttpOutbound::new();
    let routes = [(RequestId::Number(1), ResponseRoute::Session("one".into()))];
    let new_sessions = http
        .register_post_routes(&["one".into()], &routes, &permit)
        .await
        .unwrap();
    http.rollback_post_routes(&new_sessions, &routes).await;
    assert!(http.pending_routes.lock().await.is_empty());
    assert!(http.session_streams.read().await.is_empty());
}

#[tokio::test]
async fn successful_session_response_registers_stream_before_get() {
    let response = RawJsonRpcMessage::response(
        RequestId::Number(1),
        Ok(json!({"sessionId": "new-session"})),
    );
    let frame = TransportFrame::Single(response.clone());
    let (_, channel) = Channel::duplex();
    let (_, permit) = channel
        .tx
        .admission()
        .try_admit(frame.clone())
        .unwrap()
        .into_parts();
    let http = HttpOutbound::new();
    http.route_outbound_with_permit(&response, frame.to_json().unwrap(), Some(permit))
        .await
        .unwrap();
    assert!(
        http.session_streams
            .read()
            .await
            .contains_key("new-session")
    );
}
