use std::{convert::Infallible, time::Duration};

use agent_client_protocol::ConnectionLimits;
use axum::{
    Router,
    body::Body,
    http::header,
    response::{Sse, sse::Event},
    routing::{delete, get},
};
use futures::{StreamExt, channel::mpsc};
use tokio::{net::TcpListener, time::timeout};

use super::*;

async fn read_raw_sse(
    chunks: Vec<Vec<u8>>,
    max_frame_bytes: usize,
    keep_open: bool,
) -> (Result<(), String>, Vec<String>) {
    let app = Router::new().route(
        "/acp",
        get(move || {
            let chunks = chunks.clone();
            async move {
                let body = futures::stream::iter(chunks.into_iter().map(Ok::<_, Infallible>))
                    .chain(futures::stream::poll_fn(move |_| {
                        if keep_open {
                            std::task::Poll::Pending
                        } else {
                            std::task::Poll::Ready(None)
                        }
                    }));
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(body),
                )
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let connection = HttpConnection::new(
        url::Url::parse(&format!("http://{address}/acp")).unwrap(),
        reqwest::Client::new(),
    );
    connection.set_connection_id("test-connection".into());
    let (_caller, transport) = Channel::duplex_with_limits(ConnectionLimits {
        max_frame_bytes,
        max_queued_bytes: max_frame_bytes * 4,
        max_queued_frames: 4,
    });
    let (event_tx, mut event_rx) = mpsc::channel(4);
    let (established_tx, _established_rx) = futures::channel::oneshot::channel();
    let mut reader = tokio::spawn(read_sse(
        connection,
        None,
        event_tx,
        established_tx,
        transport.tx.admission(),
    ));
    let completed = timeout(Duration::from_secs(2), async {
        let mut frames = Vec::new();
        while let Some(event) = event_rx.next().await {
            frames.push(event.frame.frame().to_json().unwrap());
        }
        ((&mut reader).await.unwrap(), frames)
    })
    .await;
    reader.abort();
    server.abort();
    completed.expect("SSE reader did not finish promptly")
}

fn sse_test_json() -> String {
    TransportFrame::Single(RawJsonRpcMessage::response(
        RequestId::Number(1),
        Ok(serde_json::json!({"text": "λ"})),
    ))
    .to_json()
    .unwrap()
}

#[tokio::test]
async fn sse_repeated_cr_only_events_reset_raw_event_budget() {
    let json = sse_test_json();
    // Enough events to exceed even the raw envelope allowance if CR-only
    // blank lines fail to reset the per-event counter.
    let mut raw = format!("data: {json}\r\r").repeat(512);
    // eventsource-stream waits for a byte after the final CR to distinguish
    // it from CRLF. Start a comment without adding another data event.
    raw.push(':');
    let (result, frames) = read_raw_sse(vec![raw.into_bytes()], json.len(), false).await;
    result.unwrap();
    assert_eq!(frames, vec![json; 512]);
}

#[tokio::test]
async fn sse_exact_limit_decoded_json_allows_framing_and_split_chunks() {
    let json = sse_test_json();
    let pretty =
        serde_json::to_string_pretty(&serde_json::from_str::<serde_json::Value>(&json).unwrap())
            .unwrap();
    for eol in ["\n", "\r", "\r\n"] {
        let mut raw = String::from(": comment");
        raw.push_str(eol);
        for line in pretty.split('\n') {
            raw.push_str("data: ");
            raw.push_str(line);
            raw.push_str(eol);
        }
        raw.push_str(eol);
        raw.push(':');
        // Split inside fields, UTF-8, and CRLF terminators at the HTTP body
        // level. Deterministic limiter chunk splits are covered separately.
        let chunks = raw.as_bytes().chunks(1).map(<[u8]>::to_vec).collect();
        let (result, frames) = read_raw_sse(chunks, pretty.len(), false).await;
        result.unwrap();
        assert_eq!(
            frames.as_slice(),
            std::slice::from_ref(&json),
            "line ending {eol:?}"
        );
    }
}

#[tokio::test]
async fn sse_oversized_decoded_data_fails_before_json_admission() {
    let json = sse_test_json();
    // The canonical JSON still fits admission: the excess decoded byte is
    // whitespace that would disappear if we parsed before checking data size.
    let raw = format!("data: {json} \n\n");
    let (result, frames) = read_raw_sse(vec![raw.into_bytes()], json.len(), true).await;
    assert!(
        result
            .unwrap_err()
            .contains("SSE data exceeds maximum JSON-RPC frame size")
    );
    assert!(frames.is_empty());
}

#[tokio::test]
async fn sse_excessive_raw_comments_and_fields_fail_before_event_completes() {
    let json = sse_test_json();
    for prefix in [":", "ignored: ", "event: ", "id: ", "data: "] {
        // No blank line (or EOF): raw parser/builder accumulation must remain
        // bounded even when no decoded event is available for a payload check.
        let raw = format!("{prefix}{}", "x".repeat(32 * 1024));
        let chunks = raw.as_bytes().chunks(512).map(<[u8]>::to_vec).collect();
        let (result, frames) = read_raw_sse(chunks, json.len(), true).await;
        assert!(
            result.unwrap_err().contains("SSE event exceeds maximum"),
            "unbounded field {prefix:?}"
        );
        assert!(frames.is_empty());
    }
}

#[test]
fn sse_raw_limiter_resets_only_on_blank_lines_for_all_chunk_splits() {
    // Exact raw-envelope boundary. Repeating exceeds the event allowance unless
    // boundaries reset it; treating a CRLF's LF as a blank line is also unsafe.
    let max_frame_bytes = 1;
    let max_event_bytes = max_frame_bytes + SSE_ENVELOPE_ALLOWANCE_BYTES;
    for eol in ["\n", "\r", "\r\n"] {
        let event = format!(
            ":{}{eol}{eol}",
            "x".repeat(max_event_bytes - 1 - 2 * eol.len())
        );
        let stream = format!("{event}{event}:");
        for chunk_size in [1, 2, 3, 17, max_event_bytes - 1, max_event_bytes] {
            let mut limit = SseEventLimit::new(max_frame_bytes);
            for chunk in stream.as_bytes().chunks(chunk_size) {
                limit.check_chunk(chunk).unwrap();
            }
        }

        // Multiple completed nonblank lines still belong to one raw event.
        // Include ignored fields and comments to cover builder accumulation,
        // not merely buffering of a single long unterminated line.
        let raw = format!("ignored: x{eol}: comment{eol}").repeat(max_event_bytes / 10);
        let mut limit = SseEventLimit::new(max_frame_bytes);
        let result = raw
            .as_bytes()
            .chunks(1)
            .try_for_each(|chunk| limit.check_chunk(chunk));
        assert!(result.is_err(), "false blank line for {eol:?}");
    }
}

#[test]
fn sse_raw_limiter_rejects_one_byte_over_envelope_limit() {
    let max_frame_bytes = 1;
    let max_event_bytes = max_frame_bytes + SSE_ENVELOPE_ALLOWANCE_BYTES;
    let mut limit = SseEventLimit::new(max_frame_bytes);
    limit.check_chunk(&vec![b'x'; max_event_bytes]).unwrap();
    assert!(limit.check_chunk(b"x").is_err());
}

#[tokio::test]
async fn sse_staging_holds_shared_budget_until_frame_is_released() {
    let frame = TransportFrame::Single(
        RawJsonRpcMessage::notification("test/data".to_string(), serde_json::json!({})).unwrap(),
    );
    let json = frame.to_json().unwrap();
    let frame_bytes = json.len();
    let limits = ConnectionLimits {
        max_frame_bytes: frame_bytes + 128,
        // Leave room for one data event and reserve a whole frame for control.
        max_queued_bytes: frame_bytes + frame_bytes + 128,
        max_queued_frames: 4,
    };
    let (_caller, transport) = Channel::duplex_with_limits(limits);
    let admission = transport.tx.admission();
    let app = Router::new().route(
        "/acp",
        get({
            let json = json.clone();
            move || {
                let json = json.clone();
                async move {
                    Sse::new(futures::stream::iter((0..3).map(move |_| {
                        Ok::<_, Infallible>(Event::default().data(json.clone()))
                    })))
                }
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let connection = HttpConnection::new(
        url::Url::parse(&format!("http://{address}/acp")).unwrap(),
        reqwest::Client::new(),
    );
    connection.set_connection_id("test-connection".into());
    let (event_tx, mut event_rx) = mpsc::channel(4);
    let (established_tx, established_rx) = futures::channel::oneshot::channel();
    let reader = tokio::spawn(read_sse(
        connection,
        None,
        event_tx,
        established_tx,
        admission.clone(),
    ));
    timeout(Duration::from_secs(2), established_rx)
        .await
        .unwrap()
        .unwrap();
    let first = timeout(Duration::from_secs(2), event_rx.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.frame.frame().to_json().unwrap(), json);
    assert!(admission.try_admit(frame.clone()).is_err());
    // A second event may be parsed, but cannot enter the staging queue until
    // the first event's shared charge is released.
    assert!(
        timeout(Duration::from_millis(40), event_rx.next())
            .await
            .is_err()
    );
    drop(first);
    let second = timeout(Duration::from_secs(2), event_rx.next())
        .await
        .unwrap()
        .unwrap();
    assert!(admission.try_admit(frame.clone()).is_err());
    reader.abort();
    // Dropping the SSE reader releases even an event admitted but still
    // waiting to send; dropping the receiver releases queued events too.
    drop(second);
    drop(event_rx);
    reader.await.unwrap_err();
    let recovered = admission
        .try_admit(frame)
        .expect("cancelled SSE released permits");
    drop(recovered);
    server.abort();
}

#[tokio::test]
async fn post_and_stream_counts_are_bounded_independently_of_frame_bytes() {
    let cancellation = TransportFrame::Single(
        RawJsonRpcMessage::notification(
            "$/cancel_request".to_string(),
            serde_json::json!({"requestId": 1}),
        )
        .unwrap(),
    );
    assert!(is_cancellation_frame(&cancellation));
    assert!(!is_response_only_frame(&cancellation));
    let (_caller, transport) = Channel::duplex_with_limits(ConnectionLimits {
        max_frame_bytes: 512,
        max_queued_bytes: 4096,
        max_queued_frames: 3,
    });
    let connection = HttpConnection::new(
        url::Url::parse("http://127.0.0.1:1/acp").unwrap(),
        reqwest::Client::new(),
    );
    let mut lifecycle = HttpTransportLifecycle::new(connection, transport.tx.admission(), 3);
    for index in 0..3 {
        drop(
            lifecycle
                .begin_sse(Some(index.to_string()), mpsc::channel(1).0)
                .unwrap(),
        );
    }
    assert!(
        lifecycle
            .begin_sse(Some("excess".into()), mpsc::channel(1).0)
            .is_err()
    );
    lifecycle.sse_tasks.abort_all();
    assert_eq!(lifecycle.sse_tasks.len(), 0);

    let mut posts = PostQueues::default();
    for _ in 0..2 {
        check_post_capacity(&posts, 3, false).unwrap();
        posts.ordered.push(PendingPost {
            pending_requests: Vec::new(),
            response: Box::pin(futures::future::pending()),
        });
    }
    assert!(check_post_capacity(&posts, 3, false).is_err());
    check_post_capacity(&posts, 3, true).unwrap();
    posts.responses.push(PendingPost {
        pending_requests: Vec::new(),
        response: Box::pin(futures::future::pending()),
    });
    assert_eq!(posts.len(), 3);
    assert!(check_post_capacity(&posts, 3, true).is_err());
    drop(posts);
}

#[tokio::test]
async fn cancelled_post_releases_its_body_budget() {
    let frame = TransportFrame::Single(
        RawJsonRpcMessage::notification("test/data".to_string(), serde_json::json!({})).unwrap(),
    );
    let bytes = frame.to_json().unwrap().len();
    let (_caller, transport) = Channel::duplex_with_limits(ConnectionLimits {
        max_frame_bytes: bytes + 128,
        max_queued_bytes: bytes * 2 + 128,
        max_queued_frames: 4,
    });
    let admission = transport.tx.admission();
    let budgeted = admission.try_admit(frame.clone()).unwrap();
    let (_, permit) = budgeted.into_parts();
    let mut posts = PostQueues::default();
    posts.ordered.push_budgeted(
        PendingPost {
            pending_requests: Vec::new(),
            response: Box::pin(futures::future::pending()),
        },
        permit,
    );
    assert!(admission.try_admit(frame.clone()).is_err());
    drop(posts);
    assert!(admission.try_admit(frame).is_ok());
}

#[tokio::test]
async fn delivering_sse_preserves_admission_through_output_channel() {
    let frame = TransportFrame::Single(
        RawJsonRpcMessage::notification("test/data".to_string(), serde_json::json!({})).unwrap(),
    );
    let bytes = frame.to_json().unwrap().len();
    let (mut caller, transport) = Channel::duplex_with_limits(ConnectionLimits {
        max_frame_bytes: bytes + 128,
        max_queued_bytes: bytes * 2 + 128,
        max_queued_frames: 4,
    });
    let admission = transport.tx.admission();
    let state = ClientState {
        connection: HttpConnection::new(
            url::Url::parse("http://127.0.0.1:1/acp").unwrap(),
            reqwest::Client::new(),
        ),
        open_session_streams: HashSet::new(),
        pending_requests: HashMap::new(),
        pending_request_leases: HashMap::new(),
        incoming: transport.tx,
    };
    state
        .deliver_budgeted(admission.try_admit(frame.clone()).unwrap())
        .await
        .unwrap();
    assert!(admission.try_admit(frame.clone()).is_err());
    let delivered = caller.rx.next().await.unwrap();
    assert!(admission.try_admit(frame.clone()).is_err());
    drop(delivered);
    assert!(admission.try_admit(frame).is_ok());
}

#[tokio::test]
async fn pending_requests_hold_their_source_charge_until_terminal_response() {
    let request = RawJsonRpcMessage::request(
        "test/request".to_string(),
        serde_json::json!({}),
        RequestId::Number(1),
    )
    .unwrap();
    let frame = TransportFrame::Single(request);
    let bytes = frame.to_json().unwrap().len();
    let (_caller, transport) = Channel::duplex_with_limits(ConnectionLimits {
        max_frame_bytes: bytes + 128,
        max_queued_bytes: bytes * 2 + 128,
        max_queued_frames: 1,
    });
    let admission = transport.tx.admission();
    let mut state = ClientState {
        connection: HttpConnection::new(
            url::Url::parse("http://127.0.0.1:1/acp").unwrap(),
            reqwest::Client::new(),
        ),
        open_session_streams: HashSet::new(),
        pending_requests: HashMap::new(),
        pending_request_leases: HashMap::new(),
        incoming: transport.tx,
    };
    state.connection.set_connection_id("connection-1".into());
    let (_, permit) = admission.try_admit(frame.clone()).unwrap().into_parts();
    let post = state.prepare_frame_post(frame.clone()).unwrap().0;
    state.attach_pending_permits(&post.pending_requests, &permit);
    assert!(state.check_pending_request_capacity(1).is_err());
    drop(post);
    drop(permit);
    assert!(admission.try_admit(frame.clone()).is_err());
    assert_eq!(
        state
            .take_pending_request_method(&RequestId::Number(1))
            .as_deref(),
        Some("test/request")
    );
    assert!(state.check_pending_request_capacity(1).is_ok());
    let (_, permit) = admission.try_admit(frame.clone()).unwrap().into_parts();
    let post = state.prepare_frame_post(frame.clone()).unwrap().0;
    state.attach_pending_permits(&post.pending_requests, &permit);
    drop(post);
    drop(permit);
    let cancel = RawJsonRpcMessage::notification(
        "$/cancel_request".into(),
        serde_json::json!({"requestId": 1}),
    )
    .unwrap();
    let post = state.prepare_post(cancel).unwrap();
    handle_completed_post(
        &mut state,
        CompletedPost {
            pending_requests: post.pending_requests,
            result: Ok(()),
        },
    )
    .unwrap();
    assert!(state.check_pending_request_capacity(1).is_err());
    assert!(admission.try_admit(frame.clone()).is_err());
    assert_eq!(
        state
            .take_pending_request_method(&RequestId::Number(1))
            .as_deref(),
        Some("test/request")
    );
    assert!(state.check_pending_request_capacity(1).is_ok());
    assert!(admission.try_admit(frame).is_ok());
}

#[tokio::test]
async fn unresponsive_close_does_not_stall_transport_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/acp",
        delete(|| async { futures::future::pending::<axum::http::StatusCode>().await }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let connection = HttpConnection::new(
        url::Url::parse(&format!("http://{address}/acp")).unwrap(),
        reqwest::Client::new(),
    );
    connection.set_connection_id("test-connection".into());
    timeout(Duration::from_secs(4), connection.close())
        .await
        .expect("DELETE must not indefinitely block transport shutdown");
    server.abort();
}
