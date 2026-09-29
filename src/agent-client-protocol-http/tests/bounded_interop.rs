#![cfg(all(feature = "client", feature = "server"))]

//! Public-API interoperability over real loopback HTTP, including bounded core actors.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use agent_client_protocol::schema::{
    ProtocolVersion,
    v1::{
        ContentBlock, ContentChunk, InitializeRequest, InitializeResponse, NewSessionRequest,
        NewSessionResponse, PromptRequest, PromptResponse, SessionNotification, SessionUpdate,
        StopReason, TextContent,
    },
};
use agent_client_protocol::{Agent, ChannelLimits, Client, ConnectTo};
use agent_client_protocol_http::{AcpHttpServer, HttpClient, HttpClientLimits, ServerLimits};
use axum::{
    Router,
    body::Body,
    http::{Method, StatusCode},
    middleware,
};
use tokio::{
    net::TcpListener,
    sync::{Notify, Semaphore, oneshot},
    task::JoinHandle,
    time::timeout,
};

const DEADLINE: Duration = Duration::from_secs(30);
const CHUNK_BYTES: usize = 64 * 1024;

fn channel_limits() -> ChannelLimits {
    ChannelLimits {
        max_frame_bytes: 128 * 1024,
        max_buffered_bytes: 1024 * 1024,
        ..ChannelLimits::default()
    }
}

fn client_limits() -> HttpClientLimits {
    HttpClientLimits {
        channel: channel_limits(),
        max_buffered_bytes: 2 * 1024 * 1024,
        max_response_bytes: 128 * 1024,
        max_sse_line_bytes: 128 * 1024,
        max_sse_event_bytes: 128 * 1024,
        max_sse_chunk_bytes: 128 * 1024,
        ..HttpClientLimits::default()
    }
}

fn server_limits() -> ServerLimits {
    ServerLimits {
        channel_limits: channel_limits(),
        max_frame_bytes: 128 * 1024,
        max_egress_bytes: 256 * 1024,
        max_egress_frames: 4,
        ..ServerLimits::default()
    }
}

struct Observed {
    bytes: AtomicUsize,
    notifications: AtomicUsize,
    mutations: AtomicUsize,
    mutated: Notify,
    consumed: Semaphore,
}

impl Default for Observed {
    fn default() -> Self {
        Self {
            bytes: AtomicUsize::new(0),
            notifications: AtomicUsize::new(0),
            mutations: AtomicUsize::new(0),
            mutated: Notify::new(),
            consumed: Semaphore::new(0),
        }
    }
}

fn agent(observed: Arc<Observed>, chunks: usize) -> impl ConnectTo<Client> {
    Agent
        .builder()
        .on_receive_request(
            async |request: InitializeRequest, responder, _cx| {
                responder.respond(InitializeResponse::new(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: NewSessionRequest, responder, _cx| {
                responder.respond(NewSessionResponse::new("interop-session"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx| {
                observed.mutations.fetch_add(1, Ordering::SeqCst);
                observed.mutated.notify_one();
                for _ in 0..chunks {
                    cx.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new("x".repeat(CHUNK_BYTES)),
                        ))),
                    ))?;
                    // Test-only application consumption handshake. This is deliberately
                    // not an HTTP body poll or a claimed transport acknowledgment.
                    observed.consumed.acquire().await.unwrap().forget();
                }
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            agent_client_protocol::on_receive_request!(),
        )
}

struct NetworkServer {
    url: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<std::io::Result<()>>>,
}

impl NetworkServer {
    async fn start(router: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        Self {
            url,
            shutdown: Some(shutdown),
            task: Some(task),
        }
    }

    async fn finish(mut self) {
        let _ = self.shutdown.take().unwrap().send(());
        timeout(DEADLINE, self.task.take().unwrap())
            .await
            .expect("HTTP server did not shut down gracefully")
            .unwrap()
            .unwrap();
    }
}

impl Drop for NetworkServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn conversation(
    transport: impl ConnectTo<Client>,
    observed: Arc<Observed>,
) -> agent_client_protocol::Result<()> {
    Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                assert_eq!(notification.session_id.to_string(), "interop-session");
                if let SessionUpdate::AgentMessageChunk(chunk) = notification.update {
                    let ContentBlock::Text(text) = chunk.content else {
                        panic!("expected text");
                    };
                    assert_eq!(text.text.len(), CHUNK_BYTES);
                    observed.bytes.fetch_add(text.text.len(), Ordering::SeqCst);
                    observed.notifications.fetch_add(1, Ordering::SeqCst);
                    observed.consumed.add_permits(1);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async |cx| {
            let initialized = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert_eq!(initialized.protocol_version, ProtocolVersion::V1);
            let session = cx
                .send_request(NewSessionRequest::new(std::env::current_dir().unwrap()))
                .block_task()
                .await?;
            let response = cx
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new("mutate once"))],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await
}

async fn interoperates(bounded_server: bool, bounded_client: bool, chunks: usize) {
    let observed = Arc::new(Observed::default());
    let factory_observed = observed.clone();
    let factory = move || agent(factory_observed.clone(), chunks);
    let router = if bounded_server {
        AcpHttpServer::new_bounded(factory, server_limits())
            .unwrap()
            .into_router()
    } else {
        AcpHttpServer::new(factory).into_router()
    };
    let server = NetworkServer::start(router).await;
    let client = HttpClient::new(&server.url).unwrap();
    let result = if bounded_client {
        timeout(
            DEADLINE,
            conversation(
                client.with_limits(client_limits()).unwrap(),
                observed.clone(),
            ),
        )
        .await
    } else {
        timeout(DEADLINE, conversation(client, observed.clone())).await
    };
    result
        .expect("typed HTTP conversation or graceful client teardown stalled")
        .expect("typed HTTP conversation failed");
    assert_eq!(observed.mutations.load(Ordering::SeqCst), 1);
    assert_eq!(observed.notifications.load(Ordering::SeqCst), chunks);
    assert_eq!(observed.bytes.load(Ordering::SeqCst), chunks * CHUNK_BYTES);
    server.finish().await;
}

#[tokio::test]
async fn bounded_client_and_server_use_typed_core_and_shutdown_gracefully() {
    interoperates(true, true, 2).await;
}

#[tokio::test]
async fn bounded_server_interoperates_with_standard_http_client() {
    interoperates(true, false, 2).await;
}

#[tokio::test]
async fn bounded_client_interoperates_with_standard_http_server() {
    interoperates(false, true, 2).await;
}

#[tokio::test]
async fn healthy_session_exceeds_eight_mib_with_bounded_outstanding_work() {
    // Nine MiB over one session, while consumption releases each chunk before
    // production resumes. HTTP egress is 256 KiB and client accounting is 2 MiB:
    // these must be outstanding-work limits, never lifetime traffic limits.
    interoperates(true, true, 144).await;
}

#[tokio::test]
async fn lost_http_response_after_accepted_mutation_is_not_resubmitted() {
    let observed = Arc::new(Observed::default());
    let factory_observed = observed.clone();
    let attempts = Arc::new(AtomicUsize::new(0));
    let middleware_attempts = attempts.clone();
    let middleware_observed = observed.clone();
    let router =
        AcpHttpServer::new_bounded(move || agent(factory_observed.clone(), 0), server_limits())
            .unwrap()
            .into_router()
            .layer(middleware::from_fn(
                move |request: axum::extract::Request, next: middleware::Next| {
                    let attempts = middleware_attempts.clone();
                    let observed = middleware_observed.clone();
                    async move {
                        let mutating = request.method() == Method::POST
                            && request.headers().contains_key("acp-session-id");
                        let mut response = next.run(request).await;
                        if mutating {
                            attempts.fetch_add(1, Ordering::SeqCst);
                            assert_eq!(response.status(), StatusCode::ACCEPTED);
                            // Prove the side effect happened before destroying the HTTP result.
                            observed.mutated.notified().await;
                            *response.body_mut() =
                                Body::from_stream(futures::stream::iter([Err::<Vec<u8>, _>(
                                    std::io::Error::new(
                                        std::io::ErrorKind::ConnectionReset,
                                        "injected response loss after accepted mutation",
                                    ),
                                )]));
                        }
                        response
                    }
                },
            ));
    let server = NetworkServer::start(router).await;
    let client = HttpClient::new(&server.url)
        .unwrap()
        .with_limits(client_limits())
        .unwrap();
    let result = timeout(DEADLINE, conversation(client, observed.clone()))
        .await
        .expect("uncertain POST did not terminate");
    assert!(
        result.is_err(),
        "lost accepted HTTP response must report transport failure"
    );
    assert_eq!(
        observed.mutations.load(Ordering::SeqCst),
        1,
        "mutation was executed more than once"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "accepted POST was retried"
    );
    server.finish().await;
}
