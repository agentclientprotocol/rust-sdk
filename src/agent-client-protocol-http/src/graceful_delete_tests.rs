//! End-to-end graceful deletion tests through the real bounded ConnectTo boundary.
use super::*;
use agent_client_protocol::{Error, TransportChannel};
use futures::Future;
use std::time::Duration;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);
const NOTICE: &str = r#"{"jsonrpc":"2.0","method":"notice"}"#;

#[derive(Clone, Copy)]
enum Boundary {
    Normal,
    EarlyOutboundEof,
    CoreFailure,
}

struct BlockedAdapter {
    boundary: Boundary,
    read: oneshot::Receiver<()>,
    drained: oneshot::Sender<Vec<Vec<u8>>>,
    finish: oneshot::Receiver<bool>,
}

impl ConnectTo<Client> for BlockedAdapter {
    async fn connect_to(
        self,
        client: impl ConnectTo<agent_client_protocol::Agent>,
    ) -> agent_client_protocol::Result<()> {
        let (transport, driver) = client.into_transport_and_future();
        let TransportChannel::Bounded(mut channel) = transport else {
            panic!("graceful server must preserve bounded transport");
        };
        driver.await?;
        let init = channel.rx.next().await.expect("initialize frame");
        assert!(matches!(init.decode(), TransportFrame::Single(_)));
        drop(init);
        channel
            .tx
            .try_send_serialized(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)?;
        self.read.await.expect("release adapter reader");
        let mut received = Vec::new();
        while let Some(frame) = channel.rx.next().await {
            received.push(frame.as_bytes().to_vec());
        }
        match self.boundary {
            Boundary::Normal => {}
            Boundary::EarlyOutboundEof => channel.tx.close_channel(),
            Boundary::CoreFailure => {
                // Fail the actual shared core terminal, but return adapter success:
                // clean EOF and Ok(()) must not hide this independent failure.
                channel.tx.fail("intentional shared core terminal failure");
                self.drained.send(received).unwrap();
                return Ok(());
            }
        }
        self.drained.send(received).unwrap();
        // EOF alone is not adapter completion. Keep the outbound side alive until
        // the test explicitly permits the real component future to complete.
        let succeed = self.finish.await.expect("release adapter completion");
        drop(channel);
        if succeed {
            Ok(())
        } else {
            Err(Error::internal_error().data("intentional adapter failure"))
        }
    }
}

struct Harness {
    state: Arc<Registry>,
    id: String,
    read: oneshot::Sender<()>,
    drained: oneshot::Receiver<Vec<Vec<u8>>>,
    finish: oneshot::Sender<bool>,
}

async fn checked<F: Future>(future: F) -> F::Output {
    timeout(WAIT, future).await.expect("test made no progress")
}

fn post(body: impl Into<Body>, id: Option<&str>) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .header(header::CONTENT_TYPE, JSON_MIME_TYPE);
    if let Some(id) = id {
        request = request.header(HEADER_CONNECTION_ID, id);
    }
    request.body(body.into()).unwrap()
}

fn delete_request(id: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .header(HEADER_CONNECTION_ID, id)
        .body(Body::empty())
        .unwrap()
}

async fn setup(limits: ServerLimits, grace: Duration) -> Harness {
    setup_many(limits, grace, &[Boundary::Normal])
        .await
        .pop()
        .unwrap()
}

async fn setup_many(
    limits: ServerLimits,
    grace: Duration,
    boundaries: &[Boundary],
) -> Vec<Harness> {
    let mut controls = Vec::new();
    let mut adapters = VecDeque::new();
    for &boundary in boundaries {
        let (read, read_rx) = oneshot::channel();
        let (drained_tx, drained) = oneshot::channel();
        let (finish, finish_rx) = oneshot::channel();
        adapters.push_back(BlockedAdapter {
            boundary,
            read: read_rx,
            drained: drained_tx,
            finish: finish_rx,
        });
        controls.push((read, drained, finish));
    }
    let adapters = Mutex::new(adapters);
    let state = BoundedAcpHttpServer::new(
        move || adapters.lock().unwrap().pop_front().unwrap(),
        limits,
    )
    .unwrap()
    .with_graceful_delete(grace)
    .state;
    let mut harnesses = Vec::new();
    for (read, drained, finish) in controls {
        let id = initialize_connection(state.clone()).await;
        harnesses.push(Harness {
            state: state.clone(),
            id,
            read,
            drained,
            finish,
        });
    }
    harnesses
}

async fn initialize_connection(state: Arc<Registry>) -> String {
    let response = checked(handle_post(
        State(state.clone()),
        post(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            None,
        ),
    ))
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let id = response.headers()[HEADER_CONNECTION_ID]
        .to_str()
        .unwrap()
        .to_owned();
    // Consuming the initialization body disarms the server's cleanup guard.
    checked(axum::body::to_bytes(response.into_body(), 4096))
        .await
        .unwrap();
    id
}

async fn enqueue(h: &Harness, body: &str) {
    assert_eq!(
        checked(handle_post(
            State(h.state.clone()),
            post(body.to_owned(), Some(&h.id))
        ))
        .await
        .status(),
        StatusCode::ACCEPTED,
    );
}

async fn assert_closing_before_body_poll(state: Arc<Registry>, id: &str) {
    let body = Body::from_stream(futures::stream::poll_fn(|_| {
        panic!("a POST to a closing connection must not poll its body");
        #[allow(unreachable_code)]
        std::task::Poll::Ready(None::<Result<axum::body::Bytes, Infallible>>)
    }));
    assert_eq!(
        checked(handle_post(State(state), post(body, Some(id))))
            .await
            .status(),
        StatusCode::GONE,
    );
}

async fn full_core_drains(limits: ChannelLimits, frames: Vec<String>) {
    let h = setup(
        ServerLimits {
            channel_limits: limits,
            ..ServerLimits::default()
        },
        WAIT,
    )
    .await;
    for frame in &frames {
        enqueue(&h, frame).await;
    }
    let mut delete = Box::pin(handle_delete(State(h.state.clone()), delete_request(&h.id)));
    assert!(futures::poll!(&mut delete).is_pending());
    assert_closing_before_body_poll(h.state.clone(), &h.id).await;
    h.read.send(()).unwrap();
    let received = checked(h.drained).await.unwrap();
    assert_eq!(
        received,
        frames
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect::<Vec<_>>()
    );
    assert!(
        futures::poll!(&mut delete).is_pending(),
        "EOF must not substitute for adapter completion"
    );
    h.finish.send(true).unwrap();
    assert_eq!(checked(delete).await.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn graceful_delete_drains_a_full_core_frame_queue() {
    full_core_drains(
        ChannelLimits {
            max_buffered_frames: 2,
            ..ChannelLimits::default()
        },
        vec![NOTICE.to_owned(), NOTICE.to_owned()],
    )
    .await;
}

#[tokio::test]
async fn graceful_delete_drains_a_full_core_byte_budget() {
    let prefix = r#"{"jsonrpc":"2.0","method":"notice","params":""#;
    let frame = format!("{prefix}{}\"}}", "x".repeat(256 - prefix.len() - 2));
    assert_eq!(frame.len(), 256);
    full_core_drains(
        ChannelLimits {
            max_frame_bytes: 256,
            max_buffered_bytes: 256,
            max_buffered_frames: 8,
            ..ChannelLimits::default()
        },
        vec![frame],
    )
    .await;
}

#[tokio::test]
async fn graceful_delete_bypasses_eight_mib_of_slow_body_reservations() {
    let mut connections = setup_many(
        ServerLimits {
            max_frame_bytes: 1024 * 1024,
            max_body_bytes: 8 * 1024 * 1024,
            max_in_flight_posts: 8,
            ..ServerLimits::default()
        },
        WAIT,
        &[Boundary::Normal; 9],
    )
    .await;
    let h = connections.remove(0);
    enqueue(&h, NOTICE).await;
    let mut slow_posts = Vec::new();
    for other in &connections {
        let (polled, first_poll) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let body = Body::from_stream(futures::stream::once(async move {
            polled.send(()).unwrap();
            released.await.unwrap();
            let mut bytes = NOTICE.as_bytes().to_vec();
            bytes.resize(1024 * 1024, b' ');
            Ok::<_, Infallible>(axum::body::Bytes::from(bytes))
        }));
        let task = tokio::spawn(handle_post(
            State(h.state.clone()),
            post(body, Some(&other.id)),
        ));
        checked(first_poll).await.unwrap();
        slow_posts.push((release, task));
    }
    assert_eq!(h.state.bodies.available_permits(), 0);
    assert_eq!(h.state.posts.available_permits(), 0);
    let mut delete = Box::pin(handle_delete(State(h.state.clone()), delete_request(&h.id)));
    assert!(futures::poll!(&mut delete).is_pending());
    assert_closing_before_body_poll(h.state.clone(), &h.id).await;
    h.read.send(()).unwrap();
    assert_eq!(
        checked(h.drained).await.unwrap(),
        vec![NOTICE.as_bytes().to_vec()]
    );
    assert!(futures::poll!(&mut delete).is_pending());
    h.finish.send(true).unwrap();
    assert_eq!(checked(delete).await.status(), StatusCode::ACCEPTED);
    for (release, task) in slow_posts {
        release.send(()).unwrap();
        assert_eq!(checked(task).await.unwrap().status(), StatusCode::ACCEPTED);
    }
    assert_eq!(h.state.bodies.available_permits(), 8 * 1024 * 1024);
    for other in connections {
        let mut delete = Box::pin(handle_delete(State(other.state), delete_request(&other.id)));
        assert!(futures::poll!(&mut delete).is_pending());
        other.read.send(()).unwrap();
        assert_eq!(
            checked(other.drained).await.unwrap(),
            vec![NOTICE.as_bytes().to_vec()]
        );
        other.finish.send(true).unwrap();
        assert_eq!(checked(delete).await.status(), StatusCode::ACCEPTED);
    }
}

#[tokio::test]
async fn cancelled_delete_waiter_does_not_cancel_drain_and_repeat_joins() {
    let h = setup(ServerLimits::default(), WAIT).await;
    enqueue(&h, NOTICE).await;
    let mut first = Box::pin(handle_delete(State(h.state.clone()), delete_request(&h.id)));
    assert!(futures::poll!(&mut first).is_pending());
    drop(first);
    assert_closing_before_body_poll(h.state.clone(), &h.id).await;
    let mut repeated = Box::pin(handle_delete(State(h.state.clone()), delete_request(&h.id)));
    assert!(futures::poll!(&mut repeated).is_pending());
    h.read.send(()).unwrap();
    assert_eq!(
        checked(h.drained).await.unwrap(),
        vec![NOTICE.as_bytes().to_vec()]
    );
    assert!(futures::poll!(&mut repeated).is_pending());
    h.finish.send(true).unwrap();
    assert_eq!(checked(repeated).await.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn graceful_delete_timeout_keeps_connection_closing() {
    let h = setup(ServerLimits::default(), Duration::ZERO).await;
    enqueue(&h, NOTICE).await;
    assert_eq!(
        checked(handle_delete(State(h.state.clone()), delete_request(&h.id)))
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_closing_before_body_poll(h.state.clone(), &h.id).await;
    assert_eq!(
        checked(handle_delete(State(h.state.clone()), delete_request(&h.id)))
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    h.read.send(()).unwrap();
    assert_eq!(
        checked(h.drained).await.unwrap(),
        vec![NOTICE.as_bytes().to_vec()]
    );
    h.finish.send(true).unwrap();
}

#[tokio::test]
async fn genuine_adapter_failure_never_reports_graceful_success() {
    let h = setup(ServerLimits::default(), WAIT).await;
    enqueue(&h, NOTICE).await;
    let mut delete = Box::pin(handle_delete(State(h.state.clone()), delete_request(&h.id)));
    assert!(futures::poll!(&mut delete).is_pending());
    h.read.send(()).unwrap();
    assert_eq!(
        checked(h.drained).await.unwrap(),
        vec![NOTICE.as_bytes().to_vec()]
    );
    h.finish.send(false).unwrap();
    assert_ne!(checked(delete).await.status(), StatusCode::ACCEPTED);
    assert_ne!(
        checked(handle_delete(State(h.state), delete_request(&h.id)))
            .await
            .status(),
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn shared_core_terminal_failure_is_not_clean_eof() {
    let h = setup_many(ServerLimits::default(), WAIT, &[Boundary::CoreFailure])
        .await
        .pop()
        .unwrap();
    enqueue(&h, NOTICE).await;
    let mut delete = Box::pin(handle_delete(State(h.state.clone()), delete_request(&h.id)));
    assert!(futures::poll!(&mut delete).is_pending());
    h.read.send(()).unwrap();
    assert_eq!(
        checked(h.drained).await.unwrap(),
        vec![NOTICE.as_bytes().to_vec()]
    );
    assert_eq!(
        checked(delete).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn outbound_eof_does_not_substitute_for_adapter_completion() {
    let h = setup_many(
        ServerLimits::default(),
        Duration::ZERO,
        &[Boundary::EarlyOutboundEof],
    )
    .await
    .pop()
    .unwrap();
    enqueue(&h, NOTICE).await;
    assert_eq!(
        checked(handle_delete(State(h.state.clone()), delete_request(&h.id)))
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    h.read.send(()).unwrap();
    // This acknowledgment follows closing the actual bounded outbound direction.
    assert_eq!(
        checked(h.drained).await.unwrap(),
        vec![NOTICE.as_bytes().to_vec()]
    );
    // Give the woken router a turn to observe EOF; the adapter remains gated.
    tokio::task::yield_now().await;
    assert_eq!(
        checked(handle_delete(State(h.state.clone()), delete_request(&h.id)))
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_closing_before_body_poll(h.state.clone(), &h.id).await;
    h.finish.send(true).unwrap();
}
